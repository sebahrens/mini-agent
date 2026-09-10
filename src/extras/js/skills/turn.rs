//! Prompt-time typed discovery and immutable per-turn learned-JS binding.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use sha2::{Digest, Sha256};

use super::coordinator::IndexCoordinator;
use super::embed::{Embedder, ModelMetadata};
use super::index::{RetrievalPolicy, SkillIndex, manifest_size};
use super::router::{FrozenRoute, RouteKind, RouteRequest, route};
use super::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::extras::skills::catalog::AgentSkillCatalog;
use crate::extras::skills::index::{AgentSkillIndex, AgentSkillSearchPolicy};
use crate::extras::skills::loader::{load_resource, load_skill_markdown};
use crate::paths::AppPaths;

const MAX_QUERY_BYTES: usize = 8 * 1024;
/// Routing policy identity frozen onto every route. Recorded on the canary
/// audit record so a route can be replayed against the policy that produced it.
const ROUTE_POLICY_VERSION: &str = "phase5-v1";
/// The ten-percent Phase 5 canary ceiling, in basis points.
const CANARY_SHARE_BASIS_POINTS: u16 = 1_000;
const MAX_TRUSTED_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_AGENT_RESOURCE_CONTEXT_BYTES: usize = 32 * 1024;
const MAX_AGENT_RESOURCE_INVENTORY_BYTES: usize = 8 * 1024;
// Share live session services without making this process-wide lookup their owner.
type CoordinatorKey = (std::path::PathBuf, ModelMetadata);
type CoordinatorRegistry = Mutex<HashMap<CoordinatorKey, Weak<IndexCoordinator>>>;
static COORDINATORS: OnceLock<CoordinatorRegistry> = OnceLock::new();

struct AgentSkillState {
    catalog: Mutex<AgentSkillCatalog>,
    current: RwLock<Arc<AgentSkillIndex>>,
}

impl AgentSkillState {
    fn open(paths: &AppPaths, embedder: &Embedder) -> Result<Self, String> {
        let mut catalog = AgentSkillCatalog::new(paths);
        let current = catalog
            .refresh(embedder)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            catalog: Mutex::new(catalog),
            current: RwLock::new(Arc::new(current)),
        })
    }

    fn snapshot(&self) -> Arc<AgentSkillIndex> {
        self.current
            .read()
            .map(|index| Arc::clone(&index))
            .unwrap_or_else(|error| Arc::clone(&error.into_inner()))
    }

    fn refresh_if_changed(&self, embedder: &Embedder) -> Result<Arc<AgentSkillIndex>, String> {
        let refreshed = self
            .catalog
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .refresh_if_changed(embedder)
            .map_err(|error| error.to_string())?;
        if let Some(index) = refreshed {
            let index = Arc::new(index);
            match self.current.write() {
                Ok(mut current) => *current = Arc::clone(&index),
                Err(error) => *error.into_inner() = Arc::clone(&index),
            }
            return Ok(index);
        }
        Ok(self.snapshot())
    }
}

struct AgentSection {
    digest: String,
    markdown: String,
    resources: Vec<(String, u64, String, Option<String>)>,
    learned_js: Vec<String>,
    score: f32,
    rank: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedAgentSkill {
    pub name: String,
    pub description: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSkill {
    pub id: String,
    pub identity_version: u32,
    pub abi_version: u16,
    pub description: String,
    pub tags: Vec<String>,
    pub exports: Vec<SkillExport>,
    pub tests: Vec<String>,
    pub capability: CapabilityManifest,
    pub source: String,
    pub score_bits: u32,
    pub rank: usize,
    pub route: Option<FrozenRoute>,
}

impl ResolvedSkill {
    pub fn score(&self) -> f32 {
        f32::from_bits(self.score_bits)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnSkillBundle {
    pub turn_id: String,
    pub query_fingerprint: String,
    pub embedding_model_revision: String,
    pub index_generation: u64,
    pub skills: Vec<ResolvedSkill>,
}

impl TurnSkillBundle {
    pub fn empty(model_revision: impl Into<String>) -> Self {
        Self {
            turn_id: uuid::Uuid::new_v4().to_string(),
            query_fingerprint: String::new(),
            embedding_model_revision: model_revision.into(),
            index_generation: 0,
            skills: Vec::new(),
        }
    }
}

/// Send + Sync per-agent cell. Replacement happens only at a user-turn boundary.
/// At most this many distinct skills are attributed to one turn's outcome, so
/// a model that re-searches repeatedly cannot grow the attribution set without
/// bound.
const MAX_TURN_ATTRIBUTION_SKILLS: usize = 64;

/// Parent-owned state for the turn currently in flight.
#[derive(Default)]
struct TurnAttribution {
    turn_id: String,
    /// Every skill selected during this turn, across mid-turn re-freezes. A
    /// task outcome is attributed to this union rather than to whichever
    /// bundle happened to be current when the outcome was recorded.
    selected: Vec<String>,
    /// False once the parent knows telemetry for this turn was lost or
    /// rejected, so the turn cannot be read back as verified evidence.
    evidence_complete: bool,
}

pub struct SkillTurnContext {
    current: RwLock<Arc<TurnSkillBundle>>,
    attribution: RwLock<TurnAttribution>,
}

impl SkillTurnContext {
    pub fn new(initial: TurnSkillBundle) -> Self {
        let attribution = TurnAttribution {
            turn_id: initial.turn_id.clone(),
            selected: initial
                .skills
                .iter()
                .map(|skill| skill.id.clone())
                .take(MAX_TURN_ATTRIBUTION_SKILLS)
                .collect(),
            evidence_complete: true,
        };
        Self {
            current: RwLock::new(Arc::new(initial)),
            attribution: RwLock::new(attribution),
        }
    }

    pub fn snapshot(&self) -> Arc<TurnSkillBundle> {
        self.current
            .read()
            .map(|bundle| Arc::clone(&bundle))
            .unwrap_or_else(|error| Arc::clone(&error.into_inner()))
    }

    /// Every skill selected during the current turn.
    ///
    /// A mid-turn `skills_search` replaces the bundle but does not end the
    /// turn, so attributing an outcome to the latest bundle alone would orphan
    /// a skill that was invoked before the search.
    pub fn turn_selected_skill_ids(&self) -> Vec<String> {
        let attribution = self
            .attribution
            .read()
            .unwrap_or_else(|error| error.into_inner());
        attribution.selected.clone()
    }

    /// Whether every telemetry event this turn produced was accepted.
    pub fn evidence_complete(&self) -> bool {
        self.attribution
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .evidence_complete
    }

    /// Record that telemetry for the current turn was lost or rejected. The
    /// parent owns this bit, so a saturated or disconnected queue cannot hide
    /// the loss from the evidence it would otherwise contaminate.
    pub fn mark_evidence_lost(&self) {
        let mut attribution = self
            .attribution
            .write()
            .unwrap_or_else(|error| error.into_inner());
        attribution.evidence_complete = false;
    }

    pub fn replace(&self, bundle: TurnSkillBundle) {
        {
            let mut attribution = self
                .attribution
                .write()
                .unwrap_or_else(|error| error.into_inner());
            if attribution.turn_id != bundle.turn_id {
                // A new user turn: attribution and completeness start fresh.
                attribution.turn_id = bundle.turn_id.clone();
                attribution.selected.clear();
                attribution.evidence_complete = true;
            }
            for skill in &bundle.skills {
                if attribution.selected.len() >= MAX_TURN_ATTRIBUTION_SKILLS {
                    break;
                }
                if !attribution.selected.iter().any(|id| id == &skill.id) {
                    attribution.selected.push(skill.id.clone());
                }
            }
        }
        match self.current.write() {
            Ok(mut current) => *current = Arc::new(bundle),
            Err(error) => *error.into_inner() = Arc::new(bundle),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnDiscoveryBundle {
    pub learned_js: Arc<TurnSkillBundle>,
    pub agent_skills: Vec<ResolvedAgentSkill>,
    pub diagnostics: Vec<String>,
    pub trusted_context: String,
}

/// Runtime owner kept outside QuickJS. Query embedding occurs exactly once per call.
pub struct SkillRuntime {
    embedder: Arc<Embedder>,
    learned: Option<Arc<IndexCoordinator>>,
    agent_skills: Option<Arc<AgentSkillState>>,
    startup_diagnostics: Vec<String>,
    /// Components that could not be opened; empty when the runtime is healthy.
    degraded: Vec<String>,
    turn_context: Arc<SkillTurnContext>,
    learned_policy: RetrievalPolicy,
    agent_policy: AgentSkillSearchPolicy,
    pure_learned_only: bool,
    #[cfg(test)]
    background_rebuild_disabled: std::sync::atomic::AtomicBool,
}

impl SkillRuntime {
    #[cfg(test)]
    pub(crate) fn learned_dense_candidate_limit_for_test(&self) -> usize {
        self.learned_policy.dense_candidate_limit
    }

    /// Build both typed indexes off the request path. A failure in one domain leaves the other.
    #[cfg(test)]
    pub fn open(
        paths: &AppPaths,
        embedding_config: Option<&crate::config::EmbeddingConfig>,
    ) -> Result<Self, super::embed::EmbeddingError> {
        Self::open_with_learned_js(paths, embedding_config, true)
    }

    #[cfg(test)]
    pub(crate) fn open_with_learned_js(
        paths: &AppPaths,
        embedding_config: Option<&crate::config::EmbeddingConfig>,
        learned_js_enabled: bool,
    ) -> Result<Self, super::embed::EmbeddingError> {
        let embedder = Arc::new(Embedder::from_config(embedding_config)?);
        Self::open_with_shared_embedder(paths, embedder, learned_js_enabled, false)
    }

    /// Open a runtime on an already initialized embedding backend.
    ///
    /// `track_degradation` records components that came up unavailable so the
    /// session cache can retry them, instead of caching a runtime with an empty
    /// learned index as permanently healthy.
    pub(crate) fn open_with_shared_embedder(
        paths: &AppPaths,
        embedder: Arc<Embedder>,
        learned_js_enabled: bool,
        track_degradation: bool,
    ) -> Result<Self, super::embed::EmbeddingError> {
        let mut degraded = Vec::new();
        let mut diagnostics = Vec::new();
        let semantic_retrieval_enabled = embedder.supports_semantic_retrieval();
        if !semantic_retrieval_enabled {
            diagnostics
                .push("semantic_retrieval_unavailable:deterministic_embedding_backend".to_string());
        }
        let learned = if learned_js_enabled {
            match shared_coordinator(paths, Arc::clone(&embedder)) {
                Ok((coordinator, _created)) => Some(coordinator),
                Err(error) => {
                    // The store may be busy behind another writer, and its FTS
                    // probe reports that as a missing feature. Record it so the
                    // session cache retries rather than treating a runtime with
                    // no learned index as healthy for the whole session.
                    diagnostics.push(format!("learned_js_store_unavailable:{error}"));
                    if track_degradation {
                        degraded.push(format!("learned_index:{error}"));
                    }
                    None
                }
            }
        } else {
            diagnostics.push("learned_js_worker_containment_unavailable".to_string());
            None
        };
        let agent_skills = match AgentSkillState::open(paths, &embedder) {
            Ok(state) => Some(Arc::new(state)),
            Err(error) => {
                diagnostics.push(format!("agent_skill_catalog_unavailable:{error}"));
                if track_degradation {
                    degraded.push(format!("agent_skill_catalog:{error}"));
                }
                None
            }
        };
        let revision = embedder.model_metadata().model_revision.clone();
        let mut learned_policy = RetrievalPolicy::default();
        if !semantic_retrieval_enabled {
            learned_policy.dense_candidate_limit = 0;
        }
        // Startup diagnostics are recorded once here instead of being replayed
        // into the model-facing trusted block on every turn.
        log_skill_diagnostics(&diagnostics);
        Ok(Self {
            embedder,
            learned,
            agent_skills,
            startup_diagnostics: diagnostics,
            degraded,
            turn_context: Arc::new(SkillTurnContext::new(TurnSkillBundle::empty(revision))),
            learned_policy,
            agent_policy: AgentSkillSearchPolicy::default(),
            pure_learned_only: false,
            #[cfg(test)]
            background_rebuild_disabled: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Components that were unavailable at startup, for the session cache's
    /// bounded retry.
    pub(crate) fn degraded_components(&self) -> &[String] {
        &self.degraded
    }

    /// Whether this runtime may schedule the stale-while-revalidate rebuild.
    /// Always true in production; a test can freeze the published generation to
    /// observe the stale-lease path deterministically.
    #[cfg(test)]
    fn background_rebuild_enabled(&self) -> bool {
        !self
            .background_rebuild_disabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(not(test))]
    fn background_rebuild_enabled(&self) -> bool {
        true
    }

    /// Start a stale-while-revalidate publication without delaying runtime
    /// construction or the first prompt. Repeated callers coalesce at the
    /// process-wide coordinator.
    pub(crate) fn schedule_learned_rebuild(&self) -> bool {
        self.learned.as_ref().is_some_and(|coordinator| {
            coordinator.needs_refresh().unwrap_or(true) && coordinator.schedule_rebuild()
        })
    }

    /// Publish the first learned generation before the first prompt is prepared.
    ///
    /// The whole publication runs on a blocking worker, exactly like every other
    /// store access on this path, and the caller only waits `budget` for it. A
    /// timed-out, failed, or absent hydration returns `false` so the caller can
    /// fall back to the stale-while-revalidate scheduling path; session startup
    /// is never failed by this method.
    pub(crate) async fn hydrate_learned_index(&self, budget: std::time::Duration) -> bool {
        let Some(coordinator) = &self.learned else {
            return false;
        };
        let coordinator = Arc::clone(coordinator);
        let hydration = crate::agent::runner::spawn_blocking_scoped(move || {
            match coordinator.needs_refresh() {
                Ok(true) => coordinator.rebuild_and_publish().map(Some),
                Ok(false) => Ok(None),
                Err(error) => Err(error),
            }
        });
        match tokio::time::timeout(budget, hydration).await {
            Ok(Ok(Ok(_))) => true,
            Ok(Ok(Err(error))) => {
                tracing::warn!(%error, "learned-skill startup hydration failed");
                false
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "learned-skill startup hydration worker failed");
                false
            }
            Err(_) => {
                tracing::warn!(
                    budget_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
                    "learned-skill startup hydration exceeded its budget; \
                     falling back to the background rebuild"
                );
                false
            }
        }
    }

    pub fn turn_context(&self) -> Arc<SkillTurnContext> {
        Arc::clone(&self.turn_context)
    }

    /// Give a read-only child independent turn state while sharing immutable
    /// discovery indexes. Only active, pure learned JavaScript can enter the
    /// child's bundle; effectful skills and canary replacements are excluded.
    #[cfg(any(feature = "subagents", test))]
    pub(crate) fn fork_for_read_only_child(&self) -> Self {
        let revision = self.embedder.model_metadata().model_revision.clone();
        Self {
            embedder: Arc::clone(&self.embedder),
            learned: self.learned.clone(),
            agent_skills: self.agent_skills.clone(),
            startup_diagnostics: self.startup_diagnostics.clone(),
            degraded: self.degraded.clone(),
            turn_context: Arc::new(SkillTurnContext::new(TurnSkillBundle::empty(revision))),
            learned_policy: self.learned_policy.clone(),
            agent_policy: self.agent_policy.clone(),
            pure_learned_only: true,
            #[cfg(test)]
            background_rebuild_disabled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Freeze discovery for a new user turn.
    pub async fn prepare_turn(&self, prompt: &str) -> TurnDiscoveryBundle {
        self.discover(prompt, uuid::Uuid::new_v4().to_string())
            .await
    }

    /// Re-freeze the bound bundle for a mid-turn `skills_search` without
    /// starting a new user turn.
    ///
    /// The turn id is both the deterministic canary routing draw and the
    /// attribution key for task-outcome evidence. Minting a fresh one on every
    /// search would let a model re-roll the route until it landed on (or
    /// avoided) the canary, and would orphan every invocation already made in
    /// this user turn from the turn's outcome evidence. Only the query
    /// fingerprint and the selected bundle change here.
    pub(crate) async fn refreeze_turn(&self, query: &str) -> TurnDiscoveryBundle {
        let current = self.turn_context.snapshot().turn_id.clone();
        let turn_id = if current.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            current
        };
        self.discover(query, turn_id).await
    }

    async fn discover(&self, prompt: &str, turn_id: String) -> TurnDiscoveryBundle {
        let query = normalize_query(prompt);
        let fingerprint = fingerprint(&query);
        let mut diagnostics = self.startup_diagnostics.clone();
        let mut agent_skills = self.agent_skills.as_ref().map(|state| state.snapshot());
        if let Some(state) = &self.agent_skills {
            let state = Arc::clone(state);
            let embedder = Arc::clone(&self.embedder);
            match crate::agent::runner::spawn_blocking_scoped(move || {
                state.refresh_if_changed(&embedder)
            })
            .await
            {
                Ok(Ok(index)) => agent_skills = Some(index),
                Ok(Err(error)) => {
                    diagnostics.push(format!("agent_skill_catalog_refresh_unavailable:{error}"))
                }
                Err(error) => diagnostics.push(format!(
                    "agent_skill_catalog_refresh_worker_unavailable:{error}"
                )),
            }
        }
        let query_embedding = match self.embedder.embed_query_cached(&query).await {
            Ok(vector) => Some(vector),
            Err(error) => {
                diagnostics.push(format!("query_embedding_unavailable:{error}"));
                None
            }
        };
        let mut refresh_pending = false;
        if let Some(coordinator) = &self.learned {
            let coordinator = Arc::clone(coordinator);
            let may_schedule = self.background_rebuild_enabled();
            match crate::agent::runner::spawn_blocking_scoped(move || {
                let needs_refresh = coordinator.needs_refresh();
                if matches!(needs_refresh, Ok(true)) && may_schedule {
                    coordinator.schedule_rebuild();
                }
                (needs_refresh, coordinator.rebuild_backoff_diagnostic())
            })
            .await
            {
                Ok((Ok(true), backoff)) => {
                    refresh_pending = true;
                    diagnostics.push("learned_js_refresh_pending".to_string());
                    if let Some(error) = backoff {
                        diagnostics.push(format!("learned_js_refresh_backoff:{error}"));
                    }
                }
                Ok((Ok(false), backoff)) => {
                    if let Some(error) = backoff {
                        diagnostics.push(format!("learned_js_refresh_backoff:{error}"));
                    }
                }
                Ok((Err(error), _)) => {
                    diagnostics.push(format!("learned_js_refresh_state_unavailable:{error}"))
                }
                Err(error) => {
                    diagnostics.push(format!("learned_js_refresh_worker_unavailable:{error}"))
                }
            }
        }

        let mut learned_bundle = TurnSkillBundle {
            turn_id,
            query_fingerprint: fingerprint,
            embedding_model_revision: self.embedder.model_metadata().model_revision.clone(),
            index_generation: 0,
            skills: Vec::new(),
        };
        if let (Some(vector), Some(coordinator)) = (&query_embedding, &self.learned) {
            // `lease()` takes a std `RwLock` read guard that a concurrent
            // publication can hold while it waits on another process's SQLite
            // write lock, so it belongs on the same blocking hop as every other
            // store access on this path instead of on the async executor.
            let leased = {
                let coordinator = Arc::clone(coordinator);
                crate::agent::runner::spawn_blocking_scoped(move || coordinator.lease()).await
            };
            let leased = match leased {
                Ok(Ok(index)) => {
                    learned_bundle.index_generation = index.generation();
                    let expected = index.model().dimensions;
                    if index.model() == self.embedder.model_metadata() {
                        Some(index)
                    } else {
                        diagnostics.push(format!(
                            "learned_js_search_unavailable:{}",
                            super::index::SkillIndexError::DimensionMismatch {
                                expected,
                                actual: vector.len(),
                            }
                        ));
                        None
                    }
                }
                Ok(Err(error)) => {
                    diagnostics.push(format!("learned_js_search_unavailable:{error}"));
                    None
                }
                Err(error) => {
                    diagnostics.push(format!("learned_js_search_worker_unavailable:{error}"));
                    None
                }
            };
            if let Some(index) = leased {
                let generation = index.generation();
                let query = query.clone();
                let vector = vector.clone();
                let policy = self.learned_policy.clone();
                let pure_only = self.pure_learned_only;
                match crate::agent::runner::spawn_blocking_scoped(move || {
                    if pure_only {
                        index
                            .search_pure_with_metrics(&query, &vector, &policy)
                            .map(|output| output.skills)
                    } else {
                        index.search(&query, &vector, &policy)
                    }
                })
                .await
                {
                    Ok(Ok(skills)) => {
                        let active_ids = skills
                            .iter()
                            .map(|skill| skill.artifact.id.clone())
                            .collect::<Vec<_>>();
                        let routing_context = if self.pure_learned_only {
                            None
                        } else {
                            let coordinator = Arc::clone(coordinator);
                            crate::agent::runner::spawn_blocking_scoped(move || {
                                coordinator.routing_context(&active_ids, generation)
                            })
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .flatten()
                        };
                        let turn_id = learned_bundle.turn_id.clone();
                        learned_bundle.skills = skills
                            .into_iter()
                            .map(|skill| {
                                let candidate = routing_context.as_ref().and_then(|context| {
                                    context.candidates.get(&skill.artifact.id).cloned()
                                });
                                let route = routing_context.as_ref().and_then(|context| {
                                    route(
                                        &context.key,
                                        &RouteRequest {
                                            active_id: skill.artifact.id.clone(),
                                            active_lineage_root_id: candidate
                                                .as_ref()
                                                .map(|(_, metadata)| {
                                                    metadata.lineage_root_id.clone()
                                                })
                                                .unwrap_or_else(|| skill.artifact.id.clone()),
                                            turn_id: turn_id.clone(),
                                            policy_version: ROUTE_POLICY_VERSION.to_string(),
                                            canary_share_basis_points: CANARY_SHARE_BASIS_POINTS,
                                            retrieval_score: f64::from(skill.score),
                                            retrieval_rank: skill.rank as u32,
                                            index_generation: skill.generation,
                                            candidate: candidate
                                                .as_ref()
                                                .map(|(_, metadata)| metadata.clone()),
                                        },
                                    )
                                    .ok()
                                });
                                // Canary exposure must be auditable outside the
                                // model-facing block. The durable `skill_events`
                                // row cannot carry route identity yet, so the
                                // frozen route is recorded here instead.
                                if let Some(record) = route
                                    .as_ref()
                                    .and_then(|route| canary_route_audit(&turn_id, route))
                                {
                                    tracing::info!(
                                        route = %record,
                                        "learned-skill canary route frozen"
                                    );
                                }
                                let artifact = match (&route, candidate) {
                                    (Some(route), Some((candidate, _)))
                                        if route.chosen_id == candidate.id =>
                                    {
                                        candidate
                                    }
                                    _ => skill.artifact.as_ref().clone(),
                                };
                                resolved_skill(&artifact, skill.score, skill.rank, route)
                            })
                            .collect();
                    }
                    Ok(Err(error)) => {
                        diagnostics.push(format!("learned_js_search_unavailable:{error}"))
                    }
                    Err(error) => {
                        diagnostics.push(format!("learned_js_search_worker_unavailable:{error}"))
                    }
                }
            }
        }

        // A `--purge-learned-skill` or retirement committed by another process
        // only bumps the durable generation; this turn still holds the previous
        // lease, and the bundle carries executable source. Narrow the frozen
        // selection against durable state before publishing it so a withdrawn
        // revision can never be bound for the rest of this turn.
        if refresh_pending
            && !learned_bundle.skills.is_empty()
            && let Some(coordinator) = &self.learned
        {
            let requested = learned_bundle
                .skills
                .iter()
                .map(|skill| skill.id.clone())
                .collect::<Vec<_>>();
            let coordinator = Arc::clone(coordinator);
            let bindable = match crate::agent::runner::spawn_blocking_scoped(move || {
                coordinator.retain_bindable_ids(&requested)
            })
            .await
            {
                Ok(Ok(bindable)) => Some(bindable),
                Ok(Err(error)) => {
                    diagnostics.push(format!("learned_js_binding_state_unavailable:{error}"));
                    None
                }
                Err(error) => {
                    diagnostics.push(format!("learned_js_binding_worker_unavailable:{error}"));
                    None
                }
            };
            // Fail closed: a selection that cannot be revalidated is dropped
            // rather than bound from a superseded lease.
            let bindable = bindable.unwrap_or_default();
            learned_bundle.skills.retain(|skill| {
                let bound = bindable.contains(&skill.id);
                if !bound {
                    diagnostics.push(format!(
                        "learned_js_selection_omitted:{}:not_bindable",
                        skill.id
                    ));
                }
                bound
            });
        }

        let mut agent_skill_generation = 0;
        let mut selected_agent_skills = Vec::new();
        let mut agent_sections = Vec::new();
        if let (Some(vector), Some(index)) = (&query_embedding, &agent_skills) {
            agent_skill_generation = index.generation();
            let index = Arc::clone(index);
            let vector = vector.clone();
            let query = query.clone();
            let policy = self.agent_policy.clone();
            let semantic_retrieval_enabled = self.embedder.supports_semantic_retrieval();
            match crate::agent::runner::spawn_blocking_scoped(move || {
                let search = if semantic_retrieval_enabled {
                    index.search(&vector, &policy)
                } else {
                    index.search_lexical(&query, &policy)
                };
                search.map(|skills| {
                    let mut remaining = MAX_AGENT_RESOURCE_CONTEXT_BYTES;
                    skills
                        .into_iter()
                        .map(|skill| {
                            let markdown = load_skill_markdown(&skill.record);
                            let resources = skill
                                .record
                                .resources
                                .iter()
                                .map(|resource| {
                                    let referenced = markdown.as_ref().is_ok_and(|markdown| {
                                        markdown_references_resource(
                                            markdown,
                                            &resource.relative_path,
                                        )
                                    });
                                    let text = if referenced && resource.bytes as usize <= remaining
                                    {
                                        load_resource(&skill.record, &resource.relative_path)
                                            .ok()
                                            .and_then(|bytes| String::from_utf8(bytes).ok())
                                            .inspect(|text| {
                                                remaining = remaining.saturating_sub(text.len())
                                            })
                                    } else {
                                        None
                                    };
                                    (
                                        resource.relative_path.clone(),
                                        resource.bytes,
                                        resource.sha256.clone(),
                                        text,
                                    )
                                })
                                .collect::<Vec<_>>();
                            (skill, markdown, resources)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .await
            {
                Ok(Ok(skills)) => {
                    for (skill, markdown, resources) in skills {
                        match markdown {
                            Ok(markdown) => {
                                selected_agent_skills.push(ResolvedAgentSkill {
                                    name: skill.record.name.clone(),
                                    description: skill.record.description.clone(),
                                    digest: skill.record.digest.clone(),
                                });
                                agent_sections.push(AgentSection {
                                    digest: skill.record.digest.clone(),
                                    markdown,
                                    resources,
                                    learned_js: skill.record.learned_js.clone(),
                                    score: skill.score,
                                    rank: skill.rank,
                                });
                            }
                            Err(error) => diagnostics.push(format!(
                                "agent_skill_load_unavailable:{}:{error}",
                                skill.record.digest
                            )),
                        }
                    }
                }
                Ok(Err(error)) => {
                    diagnostics.push(format!("agent_skill_search_unavailable:{error}"))
                }
                Err(error) => {
                    diagnostics.push(format!("agent_skill_search_worker_unavailable:{error}"))
                }
            }
        }

        self.attach_declared_skills(&mut learned_bundle, &agent_sections, &mut diagnostics)
            .await;
        self.turn_context.replace(learned_bundle);
        let learned_bundle = self.turn_context.snapshot();

        let trusted_context = render_trusted_context(
            &learned_bundle,
            agent_skill_generation,
            &agent_sections,
            &mut diagnostics,
        );
        // Diagnostics stay on the returned bundle for callers but never enter the
        // model-facing block. Startup diagnostics were logged at construction, so
        // only the diagnostics this turn produced are logged here.
        let startup = self.startup_diagnostics.len().min(diagnostics.len());
        log_skill_diagnostics(&diagnostics[startup..]);
        TurnDiscoveryBundle {
            learned_js: learned_bundle,
            agent_skills: selected_agent_skills,
            diagnostics,
            trusted_context,
        }
    }

    async fn attach_declared_skills(
        &self,
        learned: &mut TurnSkillBundle,
        agent_sections: &[AgentSection],
        diagnostics: &mut Vec<String>,
    ) {
        let mut declarations = Vec::<(String, String)>::new();
        let mut seen = std::collections::HashSet::new();
        for section in agent_sections {
            for id in &section.learned_js {
                if seen.insert(id.clone()) {
                    declarations.push((section.digest.clone(), id.clone()));
                }
            }
        }
        if declarations.is_empty() {
            return;
        }
        let Some(coordinator) = &self.learned else {
            for (digest, id) in declarations {
                diagnostics.push(format!(
                    "agent_skill_learned_js_unavailable:{digest}:{id}:worker_containment"
                ));
            }
            return;
        };

        let already_selected = learned
            .skills
            .iter()
            .map(|skill| {
                skill
                    .route
                    .as_ref()
                    .map(|route| route.active_id.clone())
                    .unwrap_or_else(|| skill.id.clone())
            })
            .collect::<std::collections::HashSet<_>>();
        let unresolved = declarations
            .iter()
            .filter(|(_, id)| !already_selected.contains(id))
            .map(|(_, id)| id.clone())
            .collect::<Vec<_>>();
        let resolved = if unresolved.is_empty() {
            Vec::new()
        } else {
            let coordinator = Arc::clone(coordinator);
            let requested = unresolved.clone();
            match crate::agent::runner::spawn_blocking_scoped(move || {
                coordinator.resolve_active_ids(&requested)
            })
            .await
            {
                Ok(Ok(resolved)) => resolved,
                Ok(Err(error)) => {
                    diagnostics.push(format!("agent_skill_learned_js_store_unavailable:{error}"));
                    return;
                }
                Err(error) => {
                    diagnostics.push(format!("agent_skill_learned_js_worker_unavailable:{error}"));
                    return;
                }
            }
        };

        let mut resolved_by_id = unresolved
            .into_iter()
            .zip(resolved)
            .collect::<HashMap<String, Result<Option<SkillArtifact>, String>>>();
        let mut semantic = learned.skills.drain(..).collect::<Vec<_>>();
        let mut candidates = Vec::new();
        for (digest, id) in declarations {
            if let Some(index) = semantic.iter().position(|skill| {
                skill
                    .route
                    .as_ref()
                    .map_or_else(|| skill.id.as_str(), |route| route.active_id.as_str())
                    == id
            }) {
                candidates.push(semantic.remove(index));
                continue;
            }
            // A tampered or legacy row costs only its own declaration: every
            // other declared id in this turn still binds.
            match resolved_by_id.remove(&id) {
                Some(Ok(Some(artifact))) => {
                    if self.pure_learned_only
                        && artifact.capability.tier != super::CapabilityTier::Pure
                    {
                        diagnostics.push(format!(
                            "agent_skill_learned_js_unavailable:{digest}:{id}:not_pure"
                        ));
                    } else {
                        candidates.push(resolved_skill(&artifact, 1.0, 0, None));
                    }
                }
                Some(Err(error)) => diagnostics.push(format!(
                    "agent_skill_learned_js_unavailable:{digest}:{id}:store_error:{error}"
                )),
                Some(Ok(None)) | None => diagnostics.push(format!(
                    "agent_skill_learned_js_unavailable:{digest}:{id}:not_active"
                )),
            }
        }
        candidates.extend(semantic);

        let mut manifest_bytes = 0usize;
        let mut source_bytes = 0usize;
        for mut skill in candidates {
            if learned.skills.len() >= self.learned_policy.max_skills {
                diagnostics.push(format!(
                    "learned_js_selection_omitted:{}:skill_limit",
                    skill.id
                ));
                continue;
            }
            let artifact = SkillArtifact {
                id: skill.id.clone(),
                identity_version: skill.identity_version,
                abi_version: skill.abi_version,
                source: skill.source.clone(),
                description: skill.description.clone(),
                tags: skill.tags.clone(),
                exports: skill.exports.clone(),
                tests: skill.tests.clone(),
                capability: skill.capability.clone(),
            };
            let next_manifest = manifest_bytes.saturating_add(manifest_size(&artifact));
            let next_source = source_bytes.saturating_add(skill.source.len());
            if next_manifest > self.learned_policy.manifest_byte_budget
                || next_source > self.learned_policy.source_byte_budget
            {
                diagnostics.push(format!("learned_js_selection_omitted:{}:budget", skill.id));
                continue;
            }
            manifest_bytes = next_manifest;
            source_bytes = next_source;
            skill.rank = learned.skills.len() + 1;
            learned.skills.push(skill);
        }
    }
}

pub(crate) fn shared_coordinator(
    paths: &AppPaths,
    embedder: Arc<Embedder>,
) -> Result<(Arc<IndexCoordinator>, bool), super::coordinator::CoordinatorError> {
    let model = embedder.model_metadata();
    let key = (paths.learned_skills_db(), model.clone());
    let registry = COORDINATORS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .map_err(|_| super::coordinator::CoordinatorError::Poisoned)?;
    registry.retain(|_, coordinator| coordinator.strong_count() > 0);
    if let Some(coordinator) = registry.get(&key).and_then(Weak::upgrade) {
        return Ok((coordinator, false));
    }
    let coordinator = Arc::new(IndexCoordinator::open(paths, embedder)?);
    registry.insert(key, Arc::downgrade(&coordinator));
    Ok((coordinator, true))
}

fn resolved_skill(
    artifact: &SkillArtifact,
    score: f32,
    rank: usize,
    route: Option<FrozenRoute>,
) -> ResolvedSkill {
    ResolvedSkill {
        id: artifact.id.clone(),
        identity_version: artifact.identity_version,
        abi_version: artifact.abi_version,
        description: artifact.description.clone(),
        tags: artifact.tags.clone(),
        exports: artifact.exports.clone(),
        tests: artifact.tests.clone(),
        capability: artifact.capability.clone(),
        source: artifact.source.clone(),
        score_bits: score.to_bits(),
        rank,
        route,
    }
}

/// Render one canary-exposure audit record for a frozen route.
///
/// Any turn that had an eligible replacement candidate produces a record, so
/// both taken and untaken canary draws are auditable. The record deliberately
/// never enters the model-facing trusted block.
fn canary_route_audit(turn_id: &str, route: &FrozenRoute) -> Option<String> {
    let candidate_id = route.candidate_id.as_deref()?;
    Some(format!(
        "turn_id={turn_id} route_kind={kind} route_fingerprint={fingerprint} \
         policy_version={policy} canary_share_basis_points={share} \
         active_id={active} candidate_id={candidate_id} chosen_id={chosen} \
         index_generation={generation} fallback_before_effects={fallback}",
        kind = route_kind_token(route.route_kind),
        fingerprint = route.route_fingerprint,
        policy = route.policy_version,
        share = route.canary_share_basis_points,
        active = route.active_id,
        chosen = route.chosen_id,
        generation = route.index_generation,
        fallback = route.fallback_before_effects,
    ))
}

fn route_kind_token(kind: RouteKind) -> &'static str {
    match kind {
        RouteKind::Active => "active",
        RouteKind::Canary => "canary",
    }
}

fn normalize_query(prompt: &str) -> String {
    let mut query = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if query.len() > MAX_QUERY_BYTES {
        let mut end = MAX_QUERY_BYTES;
        while !query.is_char_boundary(end) {
            end -= 1;
        }
        query.truncate(end);
    }
    query
}

fn fingerprint(query: &str) -> String {
    let digest = Sha256::digest(query.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn markdown_references_resource(markdown: &str, relative_path: &str) -> bool {
    if markdown.contains(&format!("`{relative_path}`"))
        || markdown.contains(&format!("<{relative_path}>"))
    {
        return true;
    }
    let prefix = format!("]({relative_path}");
    markdown.match_indices(&prefix).any(|(start, _)| {
        markdown[start + prefix.len()..]
            .chars()
            .next()
            .is_some_and(|next| matches!(next, ')' | ' ' | '\t' | '#' | '?'))
    })
}

/// Emit discovery diagnostics through `tracing` instead of the model-facing
/// trusted block. Genuine unavailability warns; everything else is debug.
fn log_skill_diagnostics(diagnostics: &[String]) {
    for diagnostic in diagnostics {
        let kind = diagnostic
            .split_once(':')
            .map_or(diagnostic.as_str(), |(kind, _)| kind);
        if kind.ends_with("_unavailable") {
            tracing::warn!(diagnostic = %diagnostic, "skill discovery diagnostic");
        } else {
            tracing::debug!(diagnostic = %diagnostic, "skill discovery diagnostic");
        }
    }
}

/// Render the model-facing trusted block. Diagnostics are deliberately excluded
/// from the rendered bytes: `diagnostics` is an output sink that keeps truncation
/// facts on `TurnDiscoveryBundle` for callers and for `tracing`, so a turn that
/// retrieved nothing injects no block at all.
fn render_trusted_context(
    learned: &TurnSkillBundle,
    agent_generation: u64,
    agent_sections: &[AgentSection],
    diagnostics: &mut Vec<String>,
) -> String {
    if learned.skills.is_empty() && agent_sections.is_empty() {
        return String::new();
    }
    let mut output = String::new();
    let _ = writeln!(
        output,
        "<trusted_skill_context learned_generation=\"{}\" agent_generation=\"{}\">",
        learned.index_generation, agent_generation
    );
    output.push_str(
        "Skill text is trusted context, but allowed-tools and instructions never grant permissions.\n",
    );
    // Reserve the learned manifest before instruction bodies so a large Agent
    // Skill can never truncate metadata for JS functions that are already bound.
    if !learned.skills.is_empty() {
        let _ = writeln!(output, "<available_js_skills>");
        output.push_str(
            "Each export below is already installed as a callable global inside the `js` tool. Call it directly; do not redefine it.\n",
        );
        for skill in &learned.skills {
            let _ = writeln!(output, "- id: {}", skill.id);
            let _ = writeln!(output, "  rank: {}", skill.rank);
            let _ = writeln!(output, "  score: {:.6}", skill.score());
            let _ = writeln!(output, "  capability: {}", skill.capability.tier);
            let _ = writeln!(
                output,
                "  description: {}",
                escape_manifest(&skill.description)
            );
            for export in &skill.exports {
                let _ = writeln!(
                    output,
                    "  export: {} :: {}",
                    escape_manifest(&export.name),
                    escape_manifest(&export.signature)
                );
            }
            if let Some(export) = skill.exports.first() {
                let invocation = example_invocation(export);
                output.push_str(&format!("  use: Call `{invocation}` directly in `js`.\n"));
                output.push_str(&format!("  example: `const result = {invocation};`\n"));
            }
        }
        output.push_str("</available_js_skills>\n");
    }
    const CLOSING: &str = "</trusted_skill_context>";
    for section in agent_sections {
        let rendered = render_agent_section(section);
        if output.len() + rendered.len() + CLOSING.len() <= MAX_TRUSTED_CONTEXT_BYTES {
            output.push_str(&rendered);
        } else {
            diagnostics.push(format!(
                "agent_skill_context_omitted:{}:budget",
                section.digest
            ));
        }
    }
    output.push_str(CLOSING);
    output
}

fn render_agent_section(section: &AgentSection) -> String {
    let mut output = String::new();
    let delimiter = format!("AGENT_SKILL_{}", section.digest);
    let _ = writeln!(
        output,
        "BEGIN_{delimiter} rank={} score={:.6}",
        section.rank, section.score
    );
    output.push_str(
        &section
            .markdown
            .replace("</trusted_skill_context>", "&lt;/trusted_skill_context&gt;"),
    );
    if !section.markdown.ends_with('\n') {
        output.push('\n');
    }
    if !section.resources.is_empty() {
        output.push_str(
            "RESOURCE_INVENTORY (content is included only when referenced by SKILL.md, UTF-8, and within the turn budget):\n",
        );
        let mut inventory_bytes = 0usize;
        for (path, bytes, sha256, text) in &section.resources {
            let inventory_line = format!("- {path} bytes={bytes} sha256={sha256}\n");
            if inventory_bytes + inventory_line.len() > MAX_AGENT_RESOURCE_INVENTORY_BYTES {
                output.push_str("- [additional resource metadata omitted: inventory budget]\n");
                break;
            }
            inventory_bytes += inventory_line.len();
            output.push_str(&inventory_line);
            if let Some(text) = text {
                let resource_delimiter =
                    format!("AGENT_SKILL_RESOURCE_{}_{}", section.digest, sha256);
                let _ = writeln!(output, "BEGIN_{resource_delimiter} path={path}");
                output.push_str(
                    &text.replace("</trusted_skill_context>", "&lt;/trusted_skill_context&gt;"),
                );
                if !text.ends_with('\n') {
                    output.push('\n');
                }
                let _ = writeln!(output, "END_{resource_delimiter}");
            }
        }
    }
    let _ = writeln!(output, "END_{delimiter}");
    output
}

#[cfg(test)]
impl SkillRuntime {
    pub(crate) fn with_test_policies(
        mut self,
        learned_policy: RetrievalPolicy,
        agent_policy: AgentSkillSearchPolicy,
    ) -> Self {
        self.learned_policy = learned_policy;
        self.agent_policy = agent_policy;
        self
    }

    pub(crate) async fn embedding_cache_stats(&self) -> super::embed::CacheStats {
        self.embedder.cache_stats().await
    }

    pub(crate) fn shares_learned_coordinator(&self, other: &Self) -> bool {
        match (&self.learned, &other.learned) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }

    pub(crate) async fn settle_learned_rebuild_for_test(&self) {
        let Some(coordinator) = &self.learned else {
            return;
        };
        self.schedule_learned_rebuild();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !coordinator.rebuild_in_flight_for_test()
                    && !coordinator.needs_refresh().unwrap()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("learned-skill background rebuild should settle");
    }

    /// Freeze the published generation so a test can observe the stale-lease
    /// path deterministically instead of racing the background rebuild.
    pub(crate) fn disable_background_rebuild_for_test(&self) {
        self.background_rebuild_disabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn learned_index_len_for_test(&self) -> usize {
        self.learned
            .as_ref()
            .and_then(|coordinator| coordinator.lease().ok())
            .map_or(0, |index| index.len())
    }

    pub(crate) fn learned_rebuild_starts_for_test(&self) -> usize {
        self.learned
            .as_ref()
            .map_or(0, |coordinator| coordinator.rebuild_starts_for_test())
    }

    pub(crate) fn hold_learned_store_lock_for_test(
        &self,
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Option<std::thread::JoinHandle<()>> {
        self.learned
            .as_ref()
            .map(|coordinator| coordinator.hold_store_lock_for_test(entered, release))
    }
}

fn escape_manifest(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace(['\n', '\r'], " ")
}

fn example_invocation(export: &SkillExport) -> String {
    let has_no_arguments = export.signature.find('(').is_some_and(|start| {
        export.signature[start + 1..].find(')').is_some_and(|end| {
            export.signature[start + 1..start + 1 + end]
                .trim()
                .is_empty()
        })
    });
    let arguments = if has_no_arguments {
        ""
    } else {
        "/* arguments */"
    };
    format!("{}({arguments})", escape_manifest(&export.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::js::skills::CapabilityManifest;
    use crate::extras::js::skills::router::RouteKind;

    fn attribution_skill(id: &str) -> ResolvedSkill {
        ResolvedSkill {
            id: id.to_string(),
            identity_version: 2,
            abi_version: 2,
            description: "fixture".into(),
            tags: vec![],
            exports: vec![],
            tests: vec![],
            capability: CapabilityManifest::pure(),
            source: "function run() { return 1; }".into(),
            score_bits: 1.0f32.to_bits(),
            rank: 0,
            route: None,
        }
    }

    fn attribution_bundle(turn_id: &str, skill_ids: &[&str]) -> TurnSkillBundle {
        TurnSkillBundle {
            turn_id: turn_id.to_string(),
            query_fingerprint: "query".into(),
            embedding_model_revision: "model".into(),
            index_generation: 1,
            skills: skill_ids.iter().map(|id| attribution_skill(id)).collect(),
        }
    }

    #[test]
    fn a_mid_turn_search_keeps_earlier_selections_attributable() {
        let context = SkillTurnContext::new(attribution_bundle("turn-1", &["skill-a"]));
        // A mid-turn `skills_search` re-freezes the same turn with a different
        // bundle; the skill invoked before the search must stay attributable.
        context.replace(attribution_bundle("turn-1", &["skill-b"]));

        let mut selected = context.turn_selected_skill_ids();
        selected.sort();
        assert_eq!(selected, vec!["skill-a".to_string(), "skill-b".to_string()]);
        assert_eq!(
            context.snapshot().skills.len(),
            1,
            "the bundle still changes"
        );
    }

    #[test]
    fn an_empty_final_bundle_does_not_erase_the_turn_attribution() {
        let context = SkillTurnContext::new(attribution_bundle("turn-1", &["skill-a"]));
        context.replace(attribution_bundle("turn-1", &[]));
        assert_eq!(
            context.turn_selected_skill_ids(),
            vec!["skill-a".to_string()],
            "an empty search result must not create a false no-library turn"
        );
    }

    #[test]
    fn a_new_turn_starts_a_fresh_attribution_set() {
        let context = SkillTurnContext::new(attribution_bundle("turn-1", &["skill-a"]));
        context.replace(attribution_bundle("turn-2", &["skill-b"]));
        assert_eq!(
            context.turn_selected_skill_ids(),
            vec!["skill-b".to_string()]
        );
    }

    #[test]
    fn the_attribution_set_is_bounded() {
        let context = SkillTurnContext::new(attribution_bundle("turn-1", &[]));
        for index in 0..(MAX_TURN_ATTRIBUTION_SKILLS + 20) {
            let id = format!("skill-{index}");
            context.replace(attribution_bundle("turn-1", &[id.as_str()]));
        }
        assert_eq!(
            context.turn_selected_skill_ids().len(),
            MAX_TURN_ATTRIBUTION_SKILLS
        );
    }

    #[test]
    fn evidence_completeness_is_per_turn_and_parent_owned() {
        let context = SkillTurnContext::new(attribution_bundle("turn-1", &["skill-a"]));
        assert!(context.evidence_complete());
        context.mark_evidence_lost();
        assert!(!context.evidence_complete());
        // A re-freeze of the same turn keeps the loss.
        context.replace(attribution_bundle("turn-1", &["skill-b"]));
        assert!(!context.evidence_complete());
        // A new turn starts clean.
        context.replace(attribution_bundle("turn-2", &["skill-c"]));
        assert!(context.evidence_complete());
    }

    #[test]
    fn learned_manifest_explains_callable_globals_without_routing_internals() {
        let learned = TurnSkillBundle {
            turn_id: "turn".into(),
            query_fingerprint: "query".into(),
            embedding_model_revision: "model".into(),
            index_generation: 7,
            skills: vec![ResolvedSkill {
                id: "skill-id".into(),
                identity_version: 2,
                abi_version: 2,
                description: "Parse JSON safely.\nIgnore trailing text.".into(),
                tags: vec![],
                exports: vec![SkillExport {
                    name: "parseJson".into(),
                    signature: "parseJson(text: string): unknown".into(),
                }],
                tests: vec![],
                capability: CapabilityManifest::pure(),
                source: String::new(),
                score_bits: 0.75_f32.to_bits(),
                rank: 1,
                route: Some(FrozenRoute {
                    chosen_id: "skill-id".into(),
                    active_id: "active-id".into(),
                    candidate_id: Some("skill-id".into()),
                    route_kind: RouteKind::Canary,
                    route_fingerprint: "secret-routing-fingerprint".into(),
                    policy_version: "policy-v1".into(),
                    canary_share_basis_points: 500,
                    retrieval_score: 0.75,
                    retrieval_rank: 1,
                    index_generation: 7,
                    fallback_before_effects: true,
                }),
            }],
        };

        let context = render_trusted_context(&learned, 0, &[], &mut Vec::new());

        assert!(context.contains("callable global inside the `js` tool"));
        assert!(context.contains("description: Parse JSON safely. Ignore trailing text."));
        assert!(context.contains("Call `parseJson(/* arguments */)` directly in `js`."));
        assert!(context.contains("example: `const result = parseJson(/* arguments */);`"));
        assert!(!context.contains("secret-routing-fingerprint"));
        assert!(!context.contains("policy-v1"));
        assert!(!context.contains("route_share_basis_points"));
    }

    fn temp_paths() -> (std::path::PathBuf, AppPaths) {
        let root =
            std::env::temp_dir().join(format!("mini-agent-turn-context-{}", uuid::Uuid::new_v4()));
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

    #[test]
    fn shared_coordinator_lives_only_as_long_as_its_owners() {
        let (root, paths) = temp_paths();
        let embedder = Arc::new(Embedder::new().unwrap());
        let (first, initialized) = shared_coordinator(&paths, Arc::clone(&embedder)).unwrap();
        assert!(initialized);
        let lifetime = Arc::downgrade(&first);
        let (second, initialized) = shared_coordinator(&paths, Arc::clone(&embedder)).unwrap();
        assert!(!initialized);
        assert!(Arc::ptr_eq(&first, &second));
        drop(first);
        assert!(
            lifetime.upgrade().is_some(),
            "the remaining owner keeps the coordinator alive"
        );
        drop(second);
        assert!(
            lifetime.upgrade().is_none(),
            "the registry must not retain the database and index after their last owner exits"
        );

        let mut other_paths = paths.clone();
        other_paths.local_data_dir = root.join("another-local-data");
        let (other, _) = shared_coordinator(&other_paths, Arc::clone(&embedder)).unwrap();
        assert!(
            COORDINATORS
                .get()
                .unwrap()
                .lock()
                .unwrap()
                .values()
                .all(|cached| !Weak::ptr_eq(cached, &lifetime)),
            "a lookup must prune expired entries from other workspaces"
        );
        drop(other);

        let (reopened, initialized) = shared_coordinator(&paths, embedder).unwrap();
        assert!(
            initialized,
            "a reopened coordinator must hydrate its new index"
        );
        let reopened_lifetime = Arc::downgrade(&reopened);
        drop(reopened);
        assert!(reopened_lifetime.upgrade().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn shared_coordinator_does_not_alias_lossy_database_paths() {
        use crate::extras::js::skills::{coordinator::CoordinatorError, store::StoreError};
        use std::ffi::OsString;
        #[cfg(unix)]
        use std::os::unix::ffi::OsStringExt;
        #[cfg(windows)]
        use std::os::windows::ffi::OsStringExt;

        let (root, mut first_paths) = temp_paths();
        first_paths.local_data_dir = root.join("data-\u{fffd}");
        let mut other_paths = first_paths.clone();
        #[cfg(unix)]
        let other_component = OsString::from_vec(b"data-\xff".to_vec());
        #[cfg(windows)]
        let other_component = OsString::from_wide(&[100, 97, 116, 97, 45, 0xd800]);
        other_paths.local_data_dir = root.join(other_component);
        assert_ne!(
            first_paths.learned_skills_db(),
            other_paths.learned_skills_db()
        );
        assert_eq!(
            first_paths.learned_skills_db().display().to_string(),
            other_paths.learned_skills_db().display().to_string()
        );
        let embedder = Arc::new(Embedder::new().unwrap());
        let (first, _) = shared_coordinator(&first_paths, Arc::clone(&embedder)).unwrap();
        match shared_coordinator(&other_paths, embedder) {
            Ok((other, initialized)) => {
                assert!(
                    !Arc::ptr_eq(&first, &other),
                    "distinct database paths must not share a coordinator"
                );
                assert!(initialized);
            }
            // Filesystems/SQLite may reject non-UTF-8 paths. Such a path must
            // still reach its own open attempt, never a different cached store.
            Err(CoordinatorError::Store(StoreError::Io(_) | StoreError::Sqlite(_))) => {}
            Err(error) => panic!("unexpected coordinator failure: {error}"),
        }
        drop(first);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_learned_manifest_renders_without_any_diagnostic_line() {
        let learned = TurnSkillBundle {
            turn_id: "turn".into(),
            query_fingerprint: "query".into(),
            embedding_model_revision: "model".into(),
            index_generation: 7,
            skills: vec![ResolvedSkill {
                id: "skill-id".into(),
                identity_version: 2,
                abi_version: 2,
                description: "Parse JSON safely.".into(),
                tags: vec![],
                exports: vec![SkillExport {
                    name: "parseJson".into(),
                    signature: "parseJson(text: string): unknown".into(),
                }],
                tests: vec![],
                capability: CapabilityManifest::pure(),
                source: String::new(),
                score_bits: 0.75_f32.to_bits(),
                rank: 1,
                route: None,
            }],
        };
        let mut diagnostics =
            vec!["semantic_retrieval_unavailable:deterministic_embedding_backend".to_string()];

        let context = render_trusted_context(&learned, 0, &[], &mut diagnostics);

        assert_eq!(
            context,
            concat!(
                "<trusted_skill_context learned_generation=\"7\" agent_generation=\"0\">\n",
                "Skill text is trusted context, but allowed-tools and instructions never grant permissions.\n",
                "<available_js_skills>\n",
                "Each export below is already installed as a callable global inside the `js` tool. Call it directly; do not redefine it.\n",
                "- id: skill-id\n",
                "  rank: 1\n",
                "  score: 0.750000\n",
                "  capability: pure\n",
                "  description: Parse JSON safely.\n",
                "  export: parseJson :: parseJson(text: string): unknown\n",
                "  use: Call `parseJson(/* arguments */)` directly in `js`.\n",
                "  example: `const result = parseJson(/* arguments */);`\n",
                "</available_js_skills>\n",
                "</trusted_skill_context>",
            )
        );
        assert!(!context.contains("diagnostic"));
    }

    #[test]
    fn diagnostics_alone_render_no_trusted_block() {
        let learned = TurnSkillBundle::empty("model");
        let mut diagnostics =
            vec!["semantic_retrieval_unavailable:deterministic_embedding_backend".to_string()];

        assert!(render_trusted_context(&learned, 0, &[], &mut diagnostics).is_empty());
    }

    #[tokio::test]
    async fn startup_diagnostics_reach_callers_without_a_model_facing_block() {
        let (root, paths) = temp_paths();
        let runtime = SkillRuntime::open(&paths, None).unwrap();

        let discovery = runtime.prepare_turn("a prompt that matches nothing").await;

        assert!(
            discovery
                .diagnostics
                .iter()
                .any(|entry| entry
                    == "semantic_retrieval_unavailable:deterministic_embedding_backend"),
            "diagnostics must still reach callers: {:?}",
            discovery.diagnostics
        );
        assert!(discovery.learned_js.skills.is_empty());
        assert!(discovery.agent_skills.is_empty());
        assert!(
            discovery.trusted_context.is_empty(),
            "diagnostics must not force a model-facing block: {}",
            discovery.trusted_context
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_canary_route_is_recorded_as_an_audit_line() {
        let taken = FrozenRoute {
            chosen_id: "candidate-id".into(),
            active_id: "active-id".into(),
            candidate_id: Some("candidate-id".into()),
            route_kind: RouteKind::Canary,
            route_fingerprint: "fingerprint-abc".into(),
            policy_version: "policy-v1".into(),
            canary_share_basis_points: 500,
            retrieval_score: 0.5,
            retrieval_rank: 2,
            index_generation: 9,
            fallback_before_effects: true,
        };

        let record = canary_route_audit("turn-7", &taken)
            .expect("an eligible candidate must produce an audit record");
        assert!(record.contains("turn_id=turn-7"), "{record}");
        assert!(record.contains("route_kind=canary"), "{record}");
        assert!(
            record.contains("route_fingerprint=fingerprint-abc"),
            "{record}"
        );
        assert!(record.contains("policy_version=policy-v1"), "{record}");
        assert!(record.contains("canary_share_basis_points=500"), "{record}");
        assert!(record.contains("candidate_id=candidate-id"), "{record}");
        assert!(record.contains("chosen_id=candidate-id"), "{record}");
        assert!(record.contains("index_generation=9"), "{record}");

        let mut untaken = taken.clone();
        untaken.route_kind = RouteKind::Active;
        untaken.chosen_id = "active-id".into();
        let record = canary_route_audit("turn-7", &untaken)
            .expect("an untaken draw is still canary-exposure evidence");
        assert!(record.contains("route_kind=active"), "{record}");
        assert!(record.contains("chosen_id=active-id"), "{record}");

        let mut without_candidate = taken;
        without_candidate.candidate_id = None;
        assert!(canary_route_audit("turn-7", &without_candidate).is_none());
    }

    #[tokio::test]
    async fn a_mid_turn_refreeze_keeps_the_user_turn_id_stable() {
        let (root, paths) = temp_paths();
        let runtime = SkillRuntime::open(&paths, None).unwrap();

        let first = runtime.prepare_turn("the first user prompt").await;
        let refrozen = runtime.refreeze_turn("a mid-turn discovery query").await;
        let second = runtime.prepare_turn("the second user prompt").await;

        assert_eq!(
            first.learned_js.turn_id, refrozen.learned_js.turn_id,
            "a mid-turn search must not re-draw the turn's canary route"
        );
        assert_ne!(
            first.learned_js.query_fingerprint, refrozen.learned_js.query_fingerprint,
            "the re-frozen bundle must still describe the search query"
        );
        assert_ne!(
            first.learned_js.turn_id, second.learned_js.turn_id,
            "a new user turn must start a new attribution scope"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_pending_withdrawal_drops_the_leased_skill_before_the_bundle_is_published() {
        use crate::extras::js::skills::store::SkillStore;

        let (root, paths) = temp_paths();
        let artifact = SkillArtifact::new(
            "function purgeableTurnSkill(_cap, value) { return value.trim(); }".to_string(),
            "Normalize purgeable whitespace tokens.".to_string(),
            vec!["purgeable".to_string(), "whitespace".to_string()],
            vec![SkillExport {
                name: "purgeableTurnSkill".to_string(),
                signature: "purgeableTurnSkill(value: string): string".to_string(),
            }],
            vec!["purgeableTurnSkill(' x ') === 'x'".to_string()],
            CapabilityManifest::pure(),
        )
        .unwrap();
        SkillStore::open_at(&paths)
            .and_then(|mut store| store.insert_verified(&artifact))
            .unwrap();

        let runtime = SkillRuntime::open(&paths, None).unwrap();
        runtime.settle_learned_rebuild_for_test().await;
        let query = "normalize purgeable whitespace tokens";
        let bound = runtime.prepare_turn(query).await;
        assert!(
            bound
                .learned_js
                .skills
                .iter()
                .any(|skill| skill.id == artifact.id),
            "the fixture must be retrievable before it is withdrawn: {:?}",
            bound.diagnostics
        );

        // Another process withdraws the revision and bumps the desired
        // generation; this session still holds the previous publication.
        runtime.disable_background_rebuild_for_test();
        let mut store = SkillStore::open_at(&paths).unwrap();
        crate::extras::js::skills::retention::RetentionService::new(&mut store)
            .privacy_purge(&artifact.id, "user_request", 10)
            .unwrap();
        let state = store.generation_state().unwrap();
        store
            .request_generation(
                &state.model_id,
                &state.model_revision,
                state.dimensions,
                state.normalized,
            )
            .unwrap();
        drop(store);

        let after = runtime.prepare_turn(query).await;

        assert!(
            after.learned_js.skills.is_empty(),
            "a withdrawn revision must not be bound from a stale lease: {:?}",
            after.diagnostics
        );
        // Retrieval itself already fails closed here: with the bindable filter
        // disabled the selection is still empty for both a retirement and a
        // privacy purge, so the filter below it is defence in depth and cannot
        // be observed through this path. Asserting its diagnostic would pin
        // behaviour that never runs, so this only pins the property that
        // matters, and the filter keeps its own unit test on the coordinator.
        assert!(
            after
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic == "learned_js_refresh_pending"),
            "the stale lease must be reported as pending refresh: {:?}",
            after.diagnostics
        );
        assert!(
            !after.trusted_context.contains("purgeableTurnSkill"),
            "the withdrawn export must not reach the model-facing block"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn zero_argument_skill_example_is_directly_callable() {
        let export = SkillExport {
            name: "answer".into(),
            signature: "answer(): number".into(),
        };

        assert_eq!(example_invocation(&export), "answer()");
    }
}
