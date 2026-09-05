//! Prompt-time typed discovery and immutable per-turn learned-JS binding.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use sha2::{Digest, Sha256};

use super::coordinator::IndexCoordinator;
use super::embed::Embedder;
use super::index::{RetrievalPolicy, SkillIndex};
use super::router::{FrozenRoute, RouteRequest, route};
use super::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::extras::skills::catalog::AgentSkillCatalog;
use crate::extras::skills::index::{AgentSkillIndex, AgentSkillSearchPolicy};
use crate::extras::skills::loader::{load_resource, load_skill_markdown};
use crate::paths::AppPaths;

const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_TRUSTED_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_AGENT_RESOURCE_CONTEXT_BYTES: usize = 32 * 1024;
const MAX_AGENT_RESOURCE_INVENTORY_BYTES: usize = 8 * 1024;
type CoordinatorRegistry = Mutex<HashMap<String, Arc<IndexCoordinator>>>;
static COORDINATORS: OnceLock<CoordinatorRegistry> = OnceLock::new();

struct AgentSection {
    digest: String,
    markdown: String,
    resources: Vec<(String, u64, String, Option<String>)>,
    score: f32,
    rank: usize,
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
pub struct SkillTurnContext {
    current: RwLock<Arc<TurnSkillBundle>>,
}

impl SkillTurnContext {
    pub fn new(initial: TurnSkillBundle) -> Self {
        Self {
            current: RwLock::new(Arc::new(initial)),
        }
    }

    pub fn snapshot(&self) -> Arc<TurnSkillBundle> {
        self.current
            .read()
            .map(|bundle| Arc::clone(&bundle))
            .unwrap_or_else(|error| Arc::clone(&error.into_inner()))
    }

    pub fn replace(&self, bundle: TurnSkillBundle) {
        match self.current.write() {
            Ok(mut current) => *current = Arc::new(bundle),
            Err(error) => *error.into_inner() = Arc::new(bundle),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnDiscoveryBundle {
    pub learned_js: Arc<TurnSkillBundle>,
    pub agent_skill_generation: u64,
    pub selected_agent_digests: Vec<String>,
    pub diagnostics: Vec<String>,
    pub trusted_context: String,
}

/// Runtime owner kept outside QuickJS. Query embedding occurs exactly once per call.
pub struct SkillRuntime {
    embedder: Arc<Embedder>,
    learned: Option<Arc<IndexCoordinator>>,
    agent_skills: Option<Arc<AgentSkillIndex>>,
    startup_diagnostics: Vec<String>,
    turn_context: Arc<SkillTurnContext>,
    learned_policy: RetrievalPolicy,
    agent_policy: AgentSkillSearchPolicy,
}

impl SkillRuntime {
    /// Build both typed indexes off the request path. A failure in one domain leaves the other.
    pub fn open(
        paths: &AppPaths,
        embedding_config: Option<&crate::config::EmbeddingConfig>,
    ) -> Result<Self, super::embed::EmbeddingError> {
        Self::open_with_learned_js(paths, embedding_config, true)
    }

    pub(crate) fn open_with_learned_js(
        paths: &AppPaths,
        embedding_config: Option<&crate::config::EmbeddingConfig>,
        learned_js_enabled: bool,
    ) -> Result<Self, super::embed::EmbeddingError> {
        let embedder = Arc::new(Embedder::from_config(embedding_config)?);
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
                    diagnostics.push(format!("learned_js_store_unavailable:{error}"));
                    None
                }
            }
        } else {
            diagnostics.push("learned_js_worker_containment_unavailable".to_string());
            None
        };
        let mut catalog = AgentSkillCatalog::new(paths);
        let agent_skills = match catalog.refresh(&embedder) {
            Ok(index) => Some(Arc::new(index)),
            Err(error) => {
                diagnostics.push(format!("agent_skill_catalog_unavailable:{error}"));
                None
            }
        };
        let revision = embedder.model_metadata().model_revision.clone();
        let mut learned_policy = RetrievalPolicy::default();
        if !semantic_retrieval_enabled {
            learned_policy.dense_candidate_limit = 0;
        }
        Ok(Self {
            embedder,
            learned,
            agent_skills,
            startup_diagnostics: diagnostics,
            turn_context: Arc::new(SkillTurnContext::new(TurnSkillBundle::empty(revision))),
            learned_policy,
            agent_policy: AgentSkillSearchPolicy::default(),
        })
    }

    /// Start a stale-while-revalidate publication without delaying runtime
    /// construction or the first prompt. Repeated callers coalesce at the
    /// process-wide coordinator.
    pub(crate) fn schedule_learned_rebuild(&self) -> bool {
        self.learned.as_ref().is_some_and(|coordinator| {
            coordinator.needs_refresh().unwrap_or(true) && coordinator.schedule_rebuild()
        })
    }

    pub fn turn_context(&self) -> Arc<SkillTurnContext> {
        Arc::clone(&self.turn_context)
    }

    pub async fn prepare_turn(&self, prompt: &str) -> TurnDiscoveryBundle {
        let query = normalize_query(prompt);
        let fingerprint = fingerprint(&query);
        let mut diagnostics = self.startup_diagnostics.clone();
        let query_embedding = match self.embedder.embed_query_cached(&query).await {
            Ok(vector) => Some(vector),
            Err(error) => {
                diagnostics.push(format!("query_embedding_unavailable:{error}"));
                None
            }
        };
        if let Some(coordinator) = &self.learned {
            match coordinator.needs_refresh() {
                Ok(true) => {
                    self.schedule_learned_rebuild();
                    diagnostics.push("learned_js_refresh_pending".to_string());
                }
                Ok(false) => {}
                Err(error) => {
                    diagnostics.push(format!("learned_js_refresh_state_unavailable:{error}"))
                }
            }
        }

        let mut learned_bundle = TurnSkillBundle {
            turn_id: uuid::Uuid::new_v4().to_string(),
            query_fingerprint: fingerprint,
            embedding_model_revision: self.embedder.model_metadata().model_revision.clone(),
            index_generation: 0,
            skills: Vec::new(),
        };
        if let (Some(vector), Some(coordinator)) = (&query_embedding, &self.learned) {
            match coordinator.lease().and_then(|index| {
                learned_bundle.index_generation = index.generation();
                if index.model() != self.embedder.model_metadata() {
                    return Err(super::coordinator::CoordinatorError::Index(
                        super::index::SkillIndexError::DimensionMismatch {
                            expected: index.model().dimensions,
                            actual: vector.len(),
                        },
                    ));
                }
                Ok(index)
            }) {
                Ok(index) => {
                    let query = query.clone();
                    let vector = vector.clone();
                    let policy = self.learned_policy.clone();
                    match crate::agent::runner::spawn_blocking_scoped(move || {
                        index.search(&query, &vector, &policy)
                    })
                    .await
                    {
                        Ok(Ok(skills)) => {
                            let routing_key = coordinator.routing_key();
                            learned_bundle.skills = skills
                                .into_iter()
                                .map(|skill| {
                                    let candidate = coordinator
                                        .replacement_candidate(&skill.artifact.id, skill.generation)
                                        .ok()
                                        .flatten();
                                    let route = routing_key.as_ref().ok().and_then(|key| {
                                        route(
                                            key,
                                            &RouteRequest {
                                                active_id: skill.artifact.id.clone(),
                                                active_lineage_root_id: candidate
                                                    .as_ref()
                                                    .map(|(_, metadata)| {
                                                        metadata.lineage_root_id.clone()
                                                    })
                                                    .unwrap_or_else(|| skill.artifact.id.clone()),
                                                turn_id: learned_bundle.turn_id.clone(),
                                                policy_version: "phase5-v1".to_string(),
                                                canary_share_basis_points: 1_000,
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
                        Err(error) => diagnostics
                            .push(format!("learned_js_search_worker_unavailable:{error}")),
                    }
                }
                Err(error) => diagnostics.push(format!("learned_js_search_unavailable:{error}")),
            }
        }
        self.turn_context.replace(learned_bundle.clone());
        let learned_bundle = self.turn_context.snapshot();

        let mut agent_skill_generation = 0;
        let mut selected_agent_digests = Vec::new();
        let mut agent_sections = Vec::new();
        if let (Some(vector), Some(index)) = (&query_embedding, &self.agent_skills) {
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
                                selected_agent_digests.push(skill.record.digest.clone());
                                agent_sections.push(AgentSection {
                                    digest: skill.record.digest.clone(),
                                    markdown,
                                    resources,
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

        let trusted_context = render_trusted_context(
            &learned_bundle,
            agent_skill_generation,
            &agent_sections,
            &diagnostics,
        );
        TurnDiscoveryBundle {
            learned_js: learned_bundle,
            agent_skill_generation,
            selected_agent_digests,
            diagnostics,
            trusted_context,
        }
    }

    pub async fn prepare_prompt(&self, prompt: &str) -> String {
        let discovery = self.prepare_turn(prompt).await;
        tracing::debug!(
            learned_skill_count = discovery.learned_js.skills.len(),
            learned_generation = discovery.learned_js.index_generation,
            agent_skill_count = discovery.selected_agent_digests.len(),
            agent_generation = discovery.agent_skill_generation,
            diagnostic_count = discovery.diagnostics.len(),
            "prepared immutable prompt-time skill context"
        );
        if discovery.trusted_context.is_empty() {
            prompt.to_string()
        } else {
            format!("{}\n\n{}", discovery.trusted_context, prompt)
        }
    }
}

pub(crate) fn shared_coordinator(
    paths: &AppPaths,
    embedder: Arc<Embedder>,
) -> Result<(Arc<IndexCoordinator>, bool), super::coordinator::CoordinatorError> {
    let model = embedder.model_metadata();
    let key = format!(
        "{}\0{}\0{}\0{}\0{}",
        paths.learned_skills_db().display(),
        model.model_id,
        model.model_revision,
        model.dimensions,
        model.normalized
    );
    let registry = COORDINATORS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .map_err(|_| super::coordinator::CoordinatorError::Poisoned)?;
    if let Some(coordinator) = registry.get(&key) {
        return Ok((Arc::clone(coordinator), false));
    }
    let coordinator = Arc::new(IndexCoordinator::open(paths, embedder)?);
    registry.insert(key, Arc::clone(&coordinator));
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

fn render_trusted_context(
    learned: &TurnSkillBundle,
    agent_generation: u64,
    agent_sections: &[AgentSection],
    diagnostics: &[String],
) -> String {
    if learned.skills.is_empty() && agent_sections.is_empty() && diagnostics.is_empty() {
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
        for skill in &learned.skills {
            let _ = writeln!(output, "- id: {}", skill.id);
            let _ = writeln!(output, "  rank: {}", skill.rank);
            let _ = writeln!(output, "  score: {:.6}", skill.score());
            let _ = writeln!(output, "  capability: {}", skill.capability.tier);
            if let Some(route) = &skill.route {
                let _ = writeln!(output, "  route: {:?}", route.route_kind);
                let _ = writeln!(output, "  route_policy: {}", route.policy_version);
                let _ = writeln!(
                    output,
                    "  route_share_basis_points: {}",
                    route.canary_share_basis_points
                );
                let _ = writeln!(output, "  route_fingerprint: {}", route.route_fingerprint);
            }
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
        }
        output.push_str("</available_js_skills>\n");
    }
    const CLOSING: &str = "</trusted_skill_context>";
    for section in agent_sections {
        let rendered = render_agent_section(section);
        if output.len() + rendered.len() + CLOSING.len() <= MAX_TRUSTED_CONTEXT_BYTES {
            output.push_str(&rendered);
        } else {
            let diagnostic = format!(
                "diagnostic: agent_skill_context_omitted:{}:budget\n",
                section.digest
            );
            if output.len() + diagnostic.len() + CLOSING.len() <= MAX_TRUSTED_CONTEXT_BYTES {
                output.push_str(&diagnostic);
            }
        }
    }
    for diagnostic in diagnostics {
        let line = format!("diagnostic: {}\n", escape_manifest(diagnostic));
        if output.len() + line.len() + CLOSING.len() <= MAX_TRUSTED_CONTEXT_BYTES {
            output.push_str(&line);
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

    pub(crate) fn learned_rebuild_starts_for_test(&self) -> usize {
        self.learned
            .as_ref()
            .map_or(0, |coordinator| coordinator.rebuild_starts_for_test())
    }
}

fn escape_manifest(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace(['\n', '\r'], " ")
}
