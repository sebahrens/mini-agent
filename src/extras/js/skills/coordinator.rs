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
    routing_key_cache: RwLock<Option<CachedRoutingKey>>,
    generation_build: Mutex<()>,
    hydrated: AtomicBool,
    rebuild_in_flight: AtomicBool,
    rebuild_backoff: Mutex<RebuildBackoff>,
    #[cfg(test)]
    rebuild_starts: AtomicUsize,
}

#[derive(Clone, Copy)]
struct CachedRoutingKey {
    generation: u64,
    key: [u8; 32],
}

#[derive(Debug)]
pub(crate) struct RoutingContext {
    pub(crate) key: [u8; 32],
    pub(crate) candidates: HashMap<String, (super::SkillArtifact, CanaryCandidate)>,
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
    /// Resolve exact Agent-Skill associations without changing lifecycle state.
    ///
    /// Only identity-valid active revisions are returned. Verified or canary
    /// revisions remain unavailable until the existing human approval and
    /// explicit activation path publishes them.
    ///
    /// A tampered, legacy, or otherwise unreadable row must cost only its own
    /// declaration: every id is resolved independently and reports its own
    /// diagnostic, so one bad revision can no longer drop the whole batch.
    pub(crate) fn resolve_active_ids(
        &self,
        ids: &[String],
    ) -> Result<Vec<Result<Option<super::SkillArtifact>, String>>, CoordinatorError> {
        let store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        Ok(ids
            .iter()
            .map(|id| match store.metadata(id) {
                Ok(None) => Ok(None),
                Ok(Some(metadata)) if metadata.status != "active" => Ok(None),
                Ok(Some(_)) => store.get(id).map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            })
            .collect())
    }

    /// Narrow a leased selection to the revisions durable state still allows a
    /// turn to bind. A purge from another process deletes the row and a retire
    /// or quarantine moves it out of `active`/`canary`, so an in-flight turn
    /// that leased the previous generation must drop it before publishing its
    /// bundle. Callers must run this on a blocking worker.
    pub(crate) fn retain_bindable_ids(
        &self,
        ids: &[String],
    ) -> Result<HashSet<String>, CoordinatorError> {
        let store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        let mut bindable = HashSet::with_capacity(ids.len());
        for id in ids {
            if let Some(metadata) = store.metadata(id)?
                && matches!(metadata.status.as_str(), "active" | "canary")
            {
                bindable.insert(id.clone());
            }
        }
        Ok(bindable)
    }

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
            routing_key_cache: RwLock::new(None),
            generation_build: Mutex::new(()),
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

    /// Resolve routing state for all selected skills in one blocking database
    /// operation. Callers must run this method on a blocking worker.
    pub(crate) fn routing_context(
        &self,
        active_ids: &[String],
        expected_generation: u64,
    ) -> Result<Option<RoutingContext>, CoordinatorError> {
        let mut store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        let state = store.generation_state()?;
        if state.applied_generation != expected_generation
            || state.desired_generation != expected_generation
        {
            return Ok(None);
        }

        let key = self.routing_key_for_generation(&mut store, expected_generation)?;
        let mut candidates = HashMap::with_capacity(active_ids.len());
        for active_id in active_ids {
            if let Some(candidate) = self.replacement_candidate_locked(&store, active_id)? {
                candidates.insert(active_id.clone(), candidate);
            }
        }
        Ok(Some(RoutingContext { key, candidates }))
    }

    /// Resolve one eligible replacement canary against the exact applied
    /// generation. Synchronous callers must keep this off async executors;
    /// turn preparation uses [`Self::routing_context`] instead.
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
        self.replacement_candidate_locked(&store, active_id)
    }

    /// Resolve one eligible replacement canary. Root canaries are excluded by
    /// construction. The caller holds the store lock and has already verified
    /// that the requested generation is still current.
    fn replacement_candidate_locked(
        &self,
        store: &SkillStore,
        active_id: &str,
    ) -> Result<Option<(super::SkillArtifact, CanaryCandidate)>, CoordinatorError> {
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
                "SELECT r.id FROM skill_revisions AS r
                 WHERE r.supersedes_id = ?
                   AND r.lineage_root_id = ?
                   AND r.status = 'canary'
                 ORDER BY (
                     SELECT COUNT(*) FROM skill_events AS e
                      WHERE e.skill_id = r.id
                        AND e.event_kind = 'invoked'
                        AND e.production = 1
                        AND e.evidence_complete = 1
                 ), r.created_at, r.id
                 LIMIT 1",
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

    fn routing_key_for_generation(
        &self,
        store: &mut SkillStore,
        generation: u64,
    ) -> Result<[u8; 32], CoordinatorError> {
        use sha2::{Digest, Sha256};
        if let Some(cached) = *self
            .routing_key_cache
            .read()
            .map_err(|_| CoordinatorError::Poisoned)?
            && cached.generation == generation
        {
            return Ok(cached.key);
        }
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
        let key = bytes.try_into().map_err(|_| {
            StoreError::Constraint("canary routing key has an invalid length".to_string())
        })?;
        *self
            .routing_key_cache
            .write()
            .map_err(|_| CoordinatorError::Poisoned)? = Some(CachedRoutingKey { generation, key });
        Ok(key)
    }

    /// Load the durable routing key. Synchronous callers must keep this off
    /// async executors; turn preparation uses the generation-cached context.
    pub fn routing_key(&self) -> Result<[u8; 32], CoordinatorError> {
        let mut store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        let generation = store.generation_state()?.applied_generation;
        self.routing_key_for_generation(&mut store, generation)
    }

    /// Commit a lifecycle mutation, publish removals under the new-turn gate,
    /// then build additions off that gate. A failed rebuild leaves the verified
    /// removal-only generation published.
    pub fn coordinate_mutation<R, E>(
        &self,
        removed_ids: HashSet<String>,
        mutation: impl FnOnce(&mut SkillStore) -> Result<(R, u64), E>,
    ) -> Result<(R, PublicationReport), CoordinatedMutationError<E>> {
        // Serialize additive builds without monopolizing `store` while the
        // embedding backend performs CPU work or blocking network I/O.
        let _generation_build = self
            .generation_build
            .lock()
            .map_err(|_| CoordinatedMutationError::Publication(CoordinatorError::Poisoned))?;
        let mut store = self
            .store
            .lock()
            .map_err(|_| CoordinatedMutationError::Publication(CoordinatorError::Poisoned))?;
        let (result, generation) =
            mutation(&mut store).map_err(CoordinatedMutationError::Mutation)?;

        // Publish committed removals immediately. Additions remain invisible
        // while the complete generation is built off the new-turn gate. The
        // publication write lock is taken only now: holding it across the
        // mutation would block every reader's `lease()` for as long as the
        // SQLite write waits on another process.
        let mut published = self
            .published
            .write()
            .map_err(|_| CoordinatedMutationError::Publication(CoordinatorError::Poisoned))?;
        *published = Arc::new(published.without_ids(generation, &removed_ids));
        drop(published);
        if let Err(error) =
            store.mark_generation_applied_with_mode(generation, "removal_only", None)
        {
            // A concurrent `coordinate_removal` may have advanced the desired
            // generation past this one without taking `generation_build`. This
            // caller's lifecycle transition has already committed, so report
            // the newer removal-only generation instead of a publication error.
            if let Some(report) = superseding_removal_report(&store, generation, None) {
                return Ok((result, report));
            }
            return Err(CoordinatedMutationError::Publication(error.into()));
        }
        drop(store);

        match self.build_generation(generation) {
            Ok(snapshot) => {
                let mut store = self.store.lock().map_err(|_| {
                    CoordinatedMutationError::Publication(CoordinatorError::Poisoned)
                })?;
                if let Err(error) =
                    store.mark_generation_applied_with_mode(generation, "full", None)
                {
                    // The freshly built snapshot is stale: a concurrent removal
                    // published a newer generation while it was being built.
                    if let Some(report) = superseding_removal_report(&store, generation, None) {
                        return Ok((result, report));
                    }
                    return Err(CoordinatedMutationError::Publication(
                        CoordinatorError::from(error),
                    ));
                }
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
                let mut store = self.store.lock().map_err(|_| {
                    CoordinatedMutationError::Publication(CoordinatorError::Poisoned)
                })?;
                if let Err(mark_error) = store.mark_generation_applied_with_mode(
                    generation,
                    "removal_only",
                    Some(&diagnostic),
                ) {
                    // `build_generation` failed `ensure_generation_current`
                    // because a concurrent removal moved the generation, which
                    // also makes this acknowledgement fail. The mutation itself
                    // committed, so surface the newer removal-only generation.
                    if let Some(report) =
                        superseding_removal_report(&store, generation, Some(diagnostic.clone()))
                    {
                        return Ok((result, report));
                    }
                    return Err(CoordinatedMutationError::Publication(
                        CoordinatorError::from(mark_error),
                    ));
                }
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
        let (result, generation) =
            mutation(&mut store).map_err(CoordinatedMutationError::Mutation)?;
        // Readers keep leasing the previous generation until the mutation has
        // actually committed; the write lock is never held across SQLite I/O.
        let mut published = self
            .published
            .write()
            .map_err(|_| CoordinatedMutationError::Publication(CoordinatorError::Poisoned))?;
        *published = Arc::new(published.without_ids(generation, &removed_ids));
        drop(published);
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
        let _generation_build = self
            .generation_build
            .lock()
            .map_err(|_| CoordinatorError::Poisoned)?;
        let model = self.embedder.model_metadata().clone();
        // A mutation that already published a full generation leaves a stale
        // `needs_refresh()` behind it, so this rebuild is frequently a no-op by
        // the time it wins the build lock. Re-read durable state here instead of
        // requesting a brand-new generation and re-embedding every skill.
        {
            let store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
            let state = store.generation_state()?;
            if self.hydrated.load(Ordering::Acquire)
                && state.desired_generation == state.applied_generation
                && state.publication_mode == "full"
            {
                return Ok(state.applied_generation);
            }
        }
        let (generation, acknowledge_generation, initial, backfill, database_path) = {
            let mut store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
            let state = store.generation_state()?;
            let hydration_only = !self.hydrated.load(Ordering::Acquire)
                && state.applied_generation > 0
                && state.desired_generation == state.applied_generation
                && state.publication_mode == "full";
            let (generation, acknowledge_generation) =
                if state.desired_generation > state.applied_generation {
                    (state.desired_generation, true)
                } else if hydration_only {
                    (state.applied_generation, false)
                } else {
                    (
                        store.request_generation(
                            &model.model_id,
                            &model.model_revision,
                            model.dimensions,
                            model.normalized,
                        )?,
                        true,
                    )
                };
            let initial = store.snapshot_rows(&model.model_id, &model.model_revision)?;
            let backfill = store.embedding_backfill_rows(&model.model_id, &model.model_revision)?;
            (
                generation,
                acknowledge_generation,
                initial,
                backfill,
                store.database_path().to_path_buf(),
            )
        };

        let missing = backfill
            .iter()
            .filter(|(_, embedding)| {
                embedding
                    .as_ref()
                    .is_none_or(|embedding| !embedding_is_compatible(embedding, &model))
            })
            .map(|(artifact, _)| (artifact.id.clone(), skill_document(artifact)))
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
            let mut store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
            store.store_embedding_batch(
                &model.model_id,
                &model.model_revision,
                model.dimensions,
                model.normalized,
                &embeddings,
            )?;
        }

        let mut store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        let rows = refresh_snapshot_embeddings(&store, &model, initial)?;
        ensure_generation_current(&store, generation)?;
        let snapshot = Arc::new(ImmutableSkillIndex::build_without_ann(
            generation,
            model,
            &database_path,
            rows,
        )?);
        // Durable state must acknowledge this exact generation before readers can
        // observe it. If persistence fails, the prior Arc remains published.
        if acknowledge_generation {
            store.mark_generation_applied(generation)?;
        } else {
            ensure_generation_current(&store, generation)?;
        }
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

    fn build_generation(&self, generation: u64) -> Result<ImmutableSkillIndex, CoordinatorError> {
        let model = self.embedder.model_metadata().clone();
        let (initial, backfill, database_path) = {
            let store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
            ensure_generation_current(&store, generation)?;
            (
                store.snapshot_rows(&model.model_id, &model.model_revision)?,
                store.embedding_backfill_rows(&model.model_id, &model.model_revision)?,
                store.database_path().to_path_buf(),
            )
        };
        let missing = backfill
            .iter()
            .filter(|(_, embedding)| {
                embedding
                    .as_ref()
                    .is_none_or(|embedding| !embedding_is_compatible(embedding, &model))
            })
            .map(|(artifact, _)| (artifact.id.clone(), skill_document(artifact)))
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
            self.store
                .lock()
                .map_err(|_| CoordinatorError::Poisoned)?
                .store_embedding_batch(
                    &model.model_id,
                    &model.model_revision,
                    model.dimensions,
                    model.normalized,
                    &embeddings,
                )?;
        }
        let store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        let rows = refresh_snapshot_embeddings(&store, &model, initial)?;
        ensure_generation_current(&store, generation)?;
        ImmutableSkillIndex::build(generation, model, &database_path, rows).map_err(Into::into)
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

    #[cfg(test)]
    pub(crate) fn hold_store_lock_for_test(
        self: &Arc<Self>,
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> std::thread::JoinHandle<()> {
        let coordinator = Arc::clone(self);
        std::thread::spawn(move || {
            let _store = coordinator.store.lock().expect("test store lock");
            entered.send(()).expect("announce held test store lock");
            release.recv().expect("release held test store lock");
        })
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

/// Recover the publication report when acknowledging `generation` was refused.
///
/// `mark_generation_applied_with_mode` only matches the exact desired
/// generation. `coordinate_removal` advances that generation without taking
/// `generation_build`, so a mutation that is building a generation can find its
/// own acknowledgement refused after its lifecycle transition already
/// committed. In that case the honest answer is the newer removal-only
/// generation, not a `Publication(Constraint)` error that hides a committed
/// transition. Returns `None` when the generation was not superseded, so a
/// genuine constraint failure still reaches the caller.
fn superseding_removal_report(
    store: &SkillStore,
    generation: u64,
    diagnostic: Option<String>,
) -> Option<PublicationReport> {
    let state = store.generation_state().ok()?;
    if state.desired_generation <= generation {
        return None;
    }
    Some(PublicationReport {
        generation: state.desired_generation,
        removal_only: true,
        diagnostic,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::js::skills::store::SkillStore;
    use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
    use crate::paths::AppPaths;
    use std::sync::mpsc;

    fn temp_paths() -> (std::path::PathBuf, AppPaths) {
        let root =
            std::env::temp_dir().join(format!("mini-agent-coordinator-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local-data"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: None,
        };
        (root, paths)
    }

    fn open_coordinator(paths: &AppPaths) -> Arc<IndexCoordinator> {
        Arc::new(
            IndexCoordinator::open(
                paths,
                Arc::new(Embedder::from_config(None).expect("deterministic embedder")),
            )
            .expect("index coordinator"),
        )
    }

    fn artifact(name: &str, description: &str) -> SkillArtifact {
        SkillArtifact::new(
            format!("function {name}(_cap, value) {{ return value; }}"),
            description.to_string(),
            vec!["coordinator".to_string()],
            vec![SkillExport {
                name: name.to_string(),
                signature: format!("{name}(value: string): string"),
            }],
            vec![format!("{name}('x') === 'x'")],
            CapabilityManifest::pure(),
        )
        .expect("test artifact")
    }

    #[test]
    fn a_current_full_generation_is_not_rebuilt_and_re_embedded_again() {
        let (root, paths) = temp_paths();
        let coordinator = open_coordinator(&paths);

        let first = coordinator
            .rebuild_and_publish()
            .expect("first publication");
        let after_first = SkillStore::open_at(&paths)
            .and_then(|store| store.generation_state())
            .expect("durable state");
        let second = coordinator
            .rebuild_and_publish()
            .expect("redundant publication");
        let after_second = SkillStore::open_at(&paths)
            .and_then(|store| store.generation_state())
            .expect("durable state");

        assert_eq!(
            second, first,
            "a redundant rebuild must reuse the published generation"
        );
        assert_eq!(
            after_second.desired_generation, after_first.desired_generation,
            "a redundant rebuild must not request a brand-new generation"
        );
        assert_eq!(
            after_second.applied_generation,
            after_first.applied_generation
        );

        drop(coordinator);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_generation_superseded_by_a_concurrent_removal_reports_removal_only() {
        let (root, paths) = temp_paths();
        let coordinator = open_coordinator(&paths);
        coordinator
            .rebuild_and_publish()
            .expect("initial publication");

        let concurrent_paths = paths.clone();
        let ((requested, superseding), report) = coordinator
            .coordinate_mutation(HashSet::new(), move |store: &mut SkillStore| {
                let state = store.generation_state()?;
                let requested = store.request_generation(
                    &state.model_id,
                    &state.model_revision,
                    state.dimensions,
                    state.normalized,
                )?;
                // A `coordinate_removal` elsewhere advances the desired
                // generation without taking `generation_build`.
                let mut concurrent = SkillStore::open_at(&concurrent_paths)?;
                let superseding = concurrent.request_generation(
                    &state.model_id,
                    &state.model_revision,
                    state.dimensions,
                    state.normalized,
                )?;
                Ok::<((u64, u64), u64), StoreError>(((requested, superseding), requested))
            })
            .expect("a committed transition must not surface as a publication failure");

        assert!(
            superseding > requested,
            "the fixture must actually supersede the mutation's generation"
        );
        assert!(
            report.removal_only,
            "a superseded build must be reported as removal-only"
        );
        assert_eq!(
            report.generation, superseding,
            "the report must carry the newer durable generation"
        );

        drop(coordinator);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_current_generation_still_surfaces_a_genuine_acknowledgement_failure() {
        let (root, paths) = temp_paths();
        let mut store = SkillStore::open_at(&paths).expect("store");
        let state = store.generation_state().expect("durable state");
        let generation = store
            .request_generation(
                &state.model_id,
                &state.model_revision,
                state.dimensions,
                state.normalized,
            )
            .expect("request a generation");

        assert!(
            superseding_removal_report(&store, generation, None).is_none(),
            "a generation that is still current must not be reported as superseded"
        );

        let newer = store
            .request_generation(
                &state.model_id,
                &state.model_revision,
                state.dimensions,
                state.normalized,
            )
            .expect("request a newer generation");
        let report = superseding_removal_report(&store, generation, Some("boom".to_string()))
            .expect("a superseded generation must produce a removal-only report");
        assert_eq!(report.generation, newer);
        assert!(report.removal_only);
        assert_eq!(report.diagnostic.as_deref(), Some("boom"));

        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_in_flight_mutation_does_not_block_a_turn_lease() {
        let (root, paths) = temp_paths();
        let coordinator = open_coordinator(&paths);
        coordinator
            .rebuild_and_publish()
            .expect("initial publication");

        let (entered_tx, entered_rx) = mpsc::sync_channel::<()>(0);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let mutating = {
            let coordinator = Arc::clone(&coordinator);
            std::thread::spawn(move || {
                coordinator
                    .coordinate_mutation(HashSet::new(), move |store: &mut SkillStore| {
                        entered_tx
                            .send(())
                            .expect("announce the in-flight mutation");
                        release_rx.recv().expect("hold the mutation open");
                        let state = store.generation_state()?;
                        let generation = store.request_generation(
                            &state.model_id,
                            &state.model_revision,
                            state.dimensions,
                            state.normalized,
                        )?;
                        Ok::<((), u64), StoreError>(((), generation))
                    })
                    .map(|(_, report)| report.generation)
            })
        };
        entered_rx.recv().expect("the mutation must start");

        let (leased_tx, leased_rx) = mpsc::channel();
        let reader = Arc::clone(&coordinator);
        std::thread::spawn(move || {
            let _ = leased_tx.send(reader.lease().is_ok());
        });
        let leased = leased_rx.recv_timeout(Duration::from_secs(5));
        release_tx.send(()).expect("release the mutation");
        let generation = mutating
            .join()
            .expect("mutation thread")
            .expect("mutation should publish");

        assert_eq!(
            leased,
            Ok(true),
            "a turn lease must not wait for an in-flight lifecycle mutation"
        );
        assert!(generation > 0);

        drop(coordinator);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn one_unreadable_declared_row_does_not_drop_the_rest_of_the_batch() {
        let (root, paths) = temp_paths();
        let good = artifact("goodDeclaredSkill", "Keep a readable declared skill.");
        let tampered = artifact("tamperedDeclaredSkill", "Tamper with this row on disk.");
        let mut store = SkillStore::open_at(&paths).expect("store");
        store.insert_verified(&good).expect("insert readable row");
        store
            .insert_verified(&tampered)
            .expect("insert row to tamper with");
        store
            .connection_mut()
            .execute_batch("DROP TRIGGER IF EXISTS skill_revisions_identity_immutable;")
            .expect("drop the immutability guard for the tamper fixture");
        store
            .connection_mut()
            .execute(
                "UPDATE skill_revisions SET source = source || ' ' WHERE id = ?",
                [&tampered.id],
            )
            .expect("tamper with the stored source");
        drop(store);

        let coordinator = open_coordinator(&paths);
        let resolved = coordinator
            .resolve_active_ids(&[tampered.id.clone(), good.id.clone()])
            .expect("a tampered row must not fail the whole batch");

        assert_eq!(resolved.len(), 2);
        assert!(
            resolved[0].is_err(),
            "the tampered row must report its own diagnostic"
        );
        assert_eq!(
            resolved[1]
                .as_ref()
                .expect("readable row must still resolve")
                .as_ref()
                .map(|artifact| artifact.id.as_str()),
            Some(good.id.as_str())
        );

        drop(coordinator);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn only_active_and_canary_revisions_remain_bindable() {
        let (root, paths) = temp_paths();
        let active = artifact("activeBindableSkill", "Stay bindable while active.");
        let canary = artifact("canaryBindableSkill", "Stay bindable while a canary.");
        let retired = artifact("retiredBindableSkill", "Leave the set once retired.");
        let mut store = SkillStore::open_at(&paths).expect("store");
        store.insert_verified(&active).expect("insert active");
        store.insert_verified(&canary).expect("insert canary");
        store.insert_verified(&retired).expect("insert retired");
        store
            .connection_mut()
            .execute(
                "UPDATE skill_revisions SET status = 'canary' WHERE id = ?",
                [&canary.id],
            )
            .expect("mark the canary revision");
        let version = store
            .metadata(&retired.id)
            .expect("metadata")
            .expect("retired row")
            .row_version;
        store.retire(&retired.id, version).expect("retire");
        drop(store);

        let coordinator = open_coordinator(&paths);
        let bindable = coordinator
            .retain_bindable_ids(&[
                active.id.clone(),
                canary.id.clone(),
                retired.id.clone(),
                "purged-revision-id".to_string(),
            ])
            .expect("bindable set");

        assert!(bindable.contains(&active.id));
        assert!(
            bindable.contains(&canary.id),
            "canary routing must survive the bindable filter"
        );
        assert!(
            !bindable.contains(&retired.id),
            "a retired revision must not stay bindable"
        );
        assert!(
            !bindable.contains("purged-revision-id"),
            "a purged revision must not stay bindable"
        );
        assert_eq!(bindable.len(), 2);

        drop(coordinator);
        let _ = std::fs::remove_dir_all(root);
    }
}
