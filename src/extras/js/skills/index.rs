//! Immutable generation-stamped exact dense + FTS retrieval for learned JavaScript skills.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hnsw_rs::prelude::{DistDot, Hnsw};
use rusqlite::{Connection, OptionalExtension, params};

use super::SkillArtifact;
use super::embed::ModelMetadata;
use super::store::{SkillRecordMetadata, StoredEmbedding};

const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_QUERY_TERMS: usize = 64;
const MAX_FTS_QUERY_TERMS: usize = 16;
const LEXICAL_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "do", "for", "from", "get", "give",
    "how", "i", "in", "into", "is", "it", "me", "my", "of", "on", "or", "please", "print", "show",
    "that", "the", "this", "to", "use", "want", "with", "you",
];
#[cfg(not(test))]
const ANN_MIN_ROWS: usize = 10_000;
// Keep the normal CI smoke small while exercising the same HNSW construction/search path.
#[cfg(test)]
const ANN_MIN_ROWS: usize = 2_000;
const ANN_CONNECTIONS: usize = 24;
const ANN_CONSTRUCTION_EF: usize = 100;
const ANN_SEARCH_EF: usize = 36;
const ANN_MIN_CANDIDATES: usize = 16;
const ANN_DOT_SAFETY_SCALE: f32 = 0.9999;

#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalPolicy {
    pub max_skills: usize,
    pub dense_candidate_limit: usize,
    pub lexical_candidate_limit: usize,
    pub dense_score_floor: f32,
    pub lexical_score_floor: f32,
    pub rrf_k: f32,
    pub manifest_byte_budget: usize,
    pub source_byte_budget: usize,
}

impl Default for RetrievalPolicy {
    fn default() -> Self {
        Self {
            max_skills: 3,
            dense_candidate_limit: 16,
            lexical_candidate_limit: 24,
            dense_score_floor: 0.20,
            lexical_score_floor: 0.0,
            rrf_k: 60.0,
            manifest_byte_budget: 8 * 1024,
            source_byte_budget: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScoredSkill {
    pub artifact: Arc<SkillArtifact>,
    pub generation: u64,
    pub score: f32,
    pub dense_score: Option<f32>,
    pub lexical_score: Option<f32>,
    pub rank: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchStageDurations {
    pub dense: Duration,
    pub lexical: Duration,
    pub fusion_and_budgets: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutput {
    pub skills: Vec<ScoredSkill>,
    pub stages: SearchStageDurations,
}

pub trait SkillIndex: Send + Sync {
    fn generation(&self) -> u64;
    fn model(&self) -> &ModelMetadata;
    fn search(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        policy: &RetrievalPolicy,
    ) -> Result<Vec<ScoredSkill>, SkillIndexError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SkillIndexError {
    #[error("query exceeds the {MAX_QUERY_BYTES}-byte retrieval limit")]
    QueryTooLarge,
    #[error("query embedding dimensions mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },
    #[error("query embedding contains a non-finite value")]
    NonFiniteQuery,
    #[error("query embedding must be normalized")]
    UnnormalizedQuery,
    #[error("snapshot row {skill_id} has incompatible embedding metadata")]
    IncompatibleEmbedding { skill_id: String },
    #[error("snapshot row {skill_id} is not active or identity-valid")]
    IneligibleArtifact { skill_id: String },
    #[error("lexical index error: {0}")]
    Lexical(#[from] rusqlite::Error),
    #[error("lexical snapshot lock was poisoned")]
    LexicalPoisoned,
}

#[derive(Clone)]
struct SnapshotEntry {
    artifact: Arc<SkillArtifact>,
    lineage_key: String,
    semantic_key: String,
}

#[derive(Debug, Clone, Copy)]
struct DenseCandidate {
    index: usize,
    score: f32,
}

impl PartialEq for DenseCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.score.to_bits() == other.score.to_bits()
    }
}

impl Eq for DenseCandidate {}

impl PartialOrd for DenseCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DenseCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            // Entries are ID-sorted. At equal score, the smaller index/full ID wins.
            .then_with(|| other.index.cmp(&self.index))
    }
}

/// One complete immutable retrieval generation. Construction is off the request path.
#[derive(Clone)]
pub struct ImmutableSkillIndex {
    generation: u64,
    model: ModelMetadata,
    database_path: PathBuf,
    entries: Arc<[SnapshotEntry]>,
    embeddings: Arc<[f32]>,
    ann: Option<Arc<Hnsw<'static, f32, DistDot>>>,
    lexical: Option<Arc<Mutex<Connection>>>,
    by_id: Arc<HashMap<String, usize>>,
    hidden: Arc<HashSet<usize>>,
}

impl ImmutableSkillIndex {
    pub fn empty(generation: u64, model: ModelMetadata, database_path: PathBuf) -> Self {
        Self {
            generation,
            model,
            database_path,
            entries: Arc::from([]),
            embeddings: Arc::from([]),
            ann: None,
            lexical: None,
            by_id: Arc::new(HashMap::new()),
            hidden: Arc::new(HashSet::new()),
        }
    }

    pub fn build(
        generation: u64,
        model: ModelMetadata,
        database_path: impl AsRef<Path>,
        rows: Vec<(SkillArtifact, StoredEmbedding, SkillRecordMetadata)>,
    ) -> Result<Self, SkillIndexError> {
        Self::build_internal(generation, model, database_path, rows, true)
    }

    pub(crate) fn build_without_ann(
        generation: u64,
        model: ModelMetadata,
        database_path: impl AsRef<Path>,
        rows: Vec<(SkillArtifact, StoredEmbedding, SkillRecordMetadata)>,
    ) -> Result<Self, SkillIndexError> {
        Self::build_internal(generation, model, database_path, rows, false)
    }

    fn build_internal(
        generation: u64,
        model: ModelMetadata,
        database_path: impl AsRef<Path>,
        rows: Vec<(SkillArtifact, StoredEmbedding, SkillRecordMetadata)>,
        include_ann: bool,
    ) -> Result<Self, SkillIndexError> {
        let mut rows = rows;
        rows.sort_by(|left, right| left.0.id.cmp(&right.0.id));
        let mut entries = Vec::with_capacity(rows.len());
        let mut embeddings = Vec::with_capacity(rows.len().saturating_mul(model.dimensions));
        let mut by_id = HashMap::with_capacity(rows.len());
        for (artifact, embedding, metadata) in rows {
            artifact
                .verify_identity()
                .map_err(|_| SkillIndexError::IneligibleArtifact {
                    skill_id: artifact.id.clone(),
                })?;
            if metadata.status != "active" {
                return Err(SkillIndexError::IneligibleArtifact {
                    skill_id: artifact.id,
                });
            }
            if embedding.skill_id != artifact.id
                || embedding.model_id != model.model_id
                || embedding.model_revision != model.model_revision
                || embedding.dimensions != model.dimensions
                || !embedding.normalized
                || embedding.values.len() != model.dimensions
            {
                return Err(SkillIndexError::IncompatibleEmbedding {
                    skill_id: artifact.id,
                });
            }
            let norm_squared = embedding
                .values
                .iter()
                .map(|value| value * value)
                .sum::<f32>();
            if !embedding.values.iter().all(|value| value.is_finite())
                || (norm_squared - 1.0).abs() > 1e-3
            {
                return Err(SkillIndexError::IncompatibleEmbedding {
                    skill_id: artifact.id,
                });
            }
            let lineage_key = metadata
                .supersedes_id
                .clone()
                .unwrap_or_else(|| artifact.id.clone());
            let semantic_key = semantic_key(&artifact);
            by_id.insert(artifact.id.clone(), entries.len());
            entries.push(SnapshotEntry {
                artifact: Arc::new(artifact),
                lineage_key,
                semantic_key,
            });
            embeddings.extend(embedding.values);
        }
        let embeddings: Arc<[f32]> = embeddings.into();
        let ann = include_ann
            .then(|| build_ann(&embeddings, entries.len(), model.dimensions))
            .flatten();
        let lexical = build_lexical_snapshot(&entries)?;
        Ok(Self {
            generation,
            model,
            database_path: database_path.as_ref().to_path_buf(),
            entries: entries.into(),
            embeddings,
            ann,
            lexical,
            by_id: Arc::new(by_id),
            hidden: Arc::new(HashSet::new()),
        })
    }

    pub(crate) fn ann_recommended(&self) -> bool {
        self.entries.len() >= ANN_MIN_ROWS
    }

    pub(crate) fn with_ann(&self) -> Self {
        Self {
            generation: self.generation,
            model: self.model.clone(),
            database_path: self.database_path.clone(),
            entries: Arc::clone(&self.entries),
            embeddings: Arc::clone(&self.embeddings),
            ann: build_ann(&self.embeddings, self.entries.len(), self.model.dimensions),
            lexical: self.lexical.as_ref().map(Arc::clone),
            by_id: Arc::clone(&self.by_id),
            hidden: Arc::clone(&self.hidden),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len().saturating_sub(self.hidden.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn contains_id(&self, id: &str) -> bool {
        self.by_id
            .get(id)
            .is_some_and(|index| !self.hidden.contains(index))
    }

    pub(crate) fn without_ids(&self, generation: u64, hidden: &HashSet<String>) -> Self {
        let mut masked = self.hidden.as_ref().clone();
        for id in hidden {
            if let Some(index) = self.by_id.get(id) {
                masked.insert(*index);
            }
        }
        Self {
            generation,
            model: self.model.clone(),
            database_path: self.database_path.clone(),
            entries: Arc::clone(&self.entries),
            embeddings: Arc::clone(&self.embeddings),
            ann: self.ann.as_ref().map(Arc::clone),
            lexical: self.lexical.as_ref().map(Arc::clone),
            by_id: Arc::clone(&self.by_id),
            hidden: Arc::new(masked),
        }
    }

    pub fn search_with_metrics(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        policy: &RetrievalPolicy,
    ) -> Result<SearchOutput, SkillIndexError> {
        self.search_with_mode(query_text, query_embedding, policy, false)
    }

    /// Exact full-scan oracle retained for ANN recall and regression audits.
    pub fn search_exact_with_metrics(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        policy: &RetrievalPolicy,
    ) -> Result<SearchOutput, SkillIndexError> {
        self.search_with_mode(query_text, query_embedding, policy, true)
    }

    fn search_with_mode(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        policy: &RetrievalPolicy,
        _exact: bool,
    ) -> Result<SearchOutput, SkillIndexError> {
        validate_query(query_text, query_embedding, &self.model)?;

        let dense_started = Instant::now();
        let dense = if _exact {
            self.exact_dense_candidates(query_embedding, policy)
        } else {
            self.dense_candidates(query_embedding, policy)
        };
        let dense_duration = dense_started.elapsed();
        let lexical_started = Instant::now();
        let lexical = self.lexical_candidates(query_text, policy)?;
        let lexical_duration = lexical_started.elapsed();
        let fusion_started = Instant::now();
        let skills = self.fuse_and_budget(dense, lexical, policy);

        Ok(SearchOutput {
            skills,
            stages: SearchStageDurations {
                dense: dense_duration,
                lexical: lexical_duration,
                fusion_and_budgets: fusion_started.elapsed(),
            },
        })
    }

    fn dense_candidates(&self, query: &[f32], policy: &RetrievalPolicy) -> Vec<(usize, f32)> {
        let Some(ann) = &self.ann else {
            return self.exact_dense_candidates(query, policy);
        };
        // A visibility mask must never make request cost grow with the number of
        // retired/purged rows. Physical rebuilds compact masks; until then a
        // bounded over-fetch may return fewer candidates, but never hidden ones.
        let overfetch = if policy.dense_candidate_limit <= 10 {
            4
        } else {
            2
        };
        let candidate_count = policy
            .dense_candidate_limit
            .saturating_mul(overfetch)
            .max(ANN_MIN_CANDIDATES)
            .min(self.entries.len());
        let mut candidates = ann
            .search(query, candidate_count, ANN_SEARCH_EF.max(candidate_count))
            .into_iter()
            .filter_map(|neighbour| {
                let score = (1.0 - neighbour.distance) / ANN_DOT_SAFETY_SCALE;
                (!self.hidden.contains(&neighbour.d_id) && score >= policy.dense_score_floor)
                    .then_some((neighbour.d_id, score))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by(|(left_index, left_score), (right_index, right_score)| {
            right_score.total_cmp(left_score).then_with(|| {
                self.entries[*left_index]
                    .artifact
                    .id
                    .cmp(&self.entries[*right_index].artifact.id)
            })
        });
        candidates.truncate(policy.dense_candidate_limit.min(candidates.len()));
        candidates
    }

    fn exact_dense_candidates(&self, query: &[f32], policy: &RetrievalPolicy) -> Vec<(usize, f32)> {
        let limit = policy.dense_candidate_limit;
        if limit == 0 {
            return Vec::new();
        }
        let scores = dense_scores(
            &self.embeddings,
            self.entries.len(),
            self.model.dimensions,
            query,
        );
        let mut bounded = Vec::<DenseCandidate>::with_capacity(limit);
        for (index, score) in scores.into_iter().enumerate() {
            let candidate = DenseCandidate { index, score };
            if self.hidden.contains(&index) || candidate.score < policy.dense_score_floor {
                continue;
            }
            if bounded.len() < limit {
                bounded.push(candidate);
                if bounded.len() == limit {
                    bounded.sort_unstable_by(|left, right| right.cmp(left));
                }
            } else if bounded.last().is_some_and(|worst| candidate > *worst) {
                let last = bounded.len() - 1;
                bounded[last] = candidate;
                let mut cursor = last;
                while cursor > 0 && bounded[cursor] > bounded[cursor - 1] {
                    bounded.swap(cursor, cursor - 1);
                    cursor -= 1;
                }
            }
        }
        bounded
            .into_iter()
            .map(|candidate| (candidate.index, candidate.score))
            .collect()
    }

    fn lexical_candidates(
        &self,
        query: &str,
        policy: &RetrievalPolicy,
    ) -> Result<Vec<(usize, f32)>, SkillIndexError> {
        let Some(lexical) = &self.lexical else {
            return Ok(Vec::new());
        };
        let connection = lexical
            .lock()
            .map_err(|_| SkillIndexError::LexicalPoisoned)?;
        let Some(fts_query) = fts_query(&connection, query)? else {
            return Ok(Vec::new());
        };
        let mut statement = connection.prepare_cached(
            "SELECT id, rank
             FROM snapshot_search
             WHERE snapshot_search MATCH ?
             ORDER BY rank ASC, id ASC LIMIT ?",
        )?;
        let rows = statement.query_map(
            params![fts_query, policy.lexical_candidate_limit as i64],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        )?;
        let mut candidates = Vec::new();
        for row in rows {
            let (id, bm25) = row?;
            let Some(&index) = self.by_id.get(&id) else {
                continue;
            };
            if self.hidden.contains(&index) {
                continue;
            }
            let score = (-bm25).max(0.0) as f32;
            if score >= policy.lexical_score_floor {
                candidates.push((index, score));
            }
        }
        Ok(candidates)
    }

    fn fuse_and_budget(
        &self,
        dense: Vec<(usize, f32)>,
        lexical: Vec<(usize, f32)>,
        policy: &RetrievalPolicy,
    ) -> Vec<ScoredSkill> {
        let mut fused: BTreeMap<usize, (f32, Option<f32>, Option<f32>)> = BTreeMap::new();
        for (rank, (index, score)) in dense.iter().enumerate() {
            let entry = fused.entry(*index).or_insert((0.0, None, None));
            entry.0 += 1.0 / (policy.rrf_k + rank as f32 + 1.0);
            entry.1 = Some(*score);
        }
        for (rank, (index, score)) in lexical.iter().enumerate() {
            let entry = fused.entry(*index).or_insert((0.0, None, None));
            entry.0 += 1.0 / (policy.rrf_k + rank as f32 + 1.0);
            entry.2 = Some(*score);
        }

        let mut ranked = fused.into_iter().collect::<Vec<_>>();
        ranked.sort_unstable_by(|(left_index, left), (right_index, right)| {
            right.0.total_cmp(&left.0).then_with(|| {
                self.entries[*left_index]
                    .artifact
                    .id
                    .cmp(&self.entries[*right_index].artifact.id)
            })
        });

        let mut seen_lineages = HashSet::new();
        let mut seen_semantics = HashSet::new();
        let mut manifest_bytes = 0usize;
        let mut source_bytes = 0usize;
        let mut selected = Vec::new();
        for (index, (score, dense_score, lexical_score)) in ranked {
            if selected.len() >= policy.max_skills {
                break;
            }
            let entry = &self.entries[index];
            if !seen_lineages.insert(entry.lineage_key.clone())
                || !seen_semantics.insert(entry.semantic_key.clone())
            {
                continue;
            }
            let next_manifest = manifest_bytes.saturating_add(manifest_size(&entry.artifact));
            let next_source = source_bytes.saturating_add(entry.artifact.source.len());
            if next_manifest > policy.manifest_byte_budget
                || next_source > policy.source_byte_budget
            {
                continue;
            }
            manifest_bytes = next_manifest;
            source_bytes = next_source;
            selected.push(ScoredSkill {
                artifact: Arc::clone(&entry.artifact),
                generation: self.generation,
                score,
                dense_score,
                lexical_score,
                rank: selected.len() + 1,
            });
        }
        selected
    }
}

fn build_ann(
    embeddings: &Arc<[f32]>,
    rows: usize,
    dimensions: usize,
) -> Option<Arc<Hnsw<'static, f32, DistDot>>> {
    if rows < ANN_MIN_ROWS || dimensions == 0 {
        return None;
    }
    let mut ann =
        Hnsw::<f32, DistDot>::new(ANN_CONNECTIONS, rows, 16, ANN_CONSTRUCTION_EF, DistDot {});
    let graph_vectors = embeddings
        .iter()
        .map(|value| value * ANN_DOT_SAFETY_SCALE)
        .collect::<Vec<_>>();
    let vectors = graph_vectors
        .chunks_exact(dimensions)
        .enumerate()
        .map(|(index, vector)| (vector, index))
        .collect::<Vec<_>>();
    ann.parallel_insert_slice(&vectors);
    ann.set_searching_mode(true);
    Some(Arc::new(ann))
}

fn build_lexical_snapshot(
    entries: &[SnapshotEntry],
) -> Result<Option<Arc<Mutex<Connection>>>, SkillIndexError> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut connection = Connection::open_in_memory()?;
    connection.execute_batch(
        "CREATE VIRTUAL TABLE snapshot_search USING fts5(
            id UNINDEXED,
            identifier,
            description,
            tags,
            exports,
            tokenize = 'unicode61'
        );
        CREATE VIRTUAL TABLE snapshot_vocab USING fts5vocab(snapshot_search, 'row');",
    )?;
    {
        let transaction = connection.transaction()?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO snapshot_search (id, identifier, description, tags, exports)
                 VALUES (?, ?, ?, ?, ?)",
            )?;
            for entry in entries {
                let identifiers = entry
                    .artifact
                    .exports
                    .iter()
                    .map(|export| export.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                let exports = entry
                    .artifact
                    .exports
                    .iter()
                    .map(|export| format!("{} {}", export.name, export.signature))
                    .collect::<Vec<_>>()
                    .join(" ");
                insert.execute(params![
                    entry.artifact.id,
                    identifiers,
                    entry.artifact.description,
                    entry.artifact.tags.join(" "),
                    exports,
                ])?;
            }
        }
        transaction.commit()?;
    }
    Ok(Some(Arc::new(Mutex::new(connection))))
}

impl SkillIndex for ImmutableSkillIndex {
    fn generation(&self) -> u64 {
        self.generation
    }

    fn model(&self) -> &ModelMetadata {
        &self.model
    }

    fn search(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        policy: &RetrievalPolicy,
    ) -> Result<Vec<ScoredSkill>, SkillIndexError> {
        self.search_with_metrics(query_text, query_embedding, policy)
            .map(|output| output.skills)
    }
}

fn validate_query(
    query_text: &str,
    query_embedding: &[f32],
    model: &ModelMetadata,
) -> Result<(), SkillIndexError> {
    if query_text.len() > MAX_QUERY_BYTES {
        return Err(SkillIndexError::QueryTooLarge);
    }
    if query_embedding.len() != model.dimensions {
        return Err(SkillIndexError::DimensionMismatch {
            expected: model.dimensions,
            actual: query_embedding.len(),
        });
    }
    if !query_embedding.iter().all(|value| value.is_finite()) {
        return Err(SkillIndexError::NonFiniteQuery);
    }
    let norm_squared: f32 = query_embedding.iter().map(|value| value * value).sum();
    if (norm_squared - 1.0).abs() > 1e-3 {
        return Err(SkillIndexError::UnnormalizedQuery);
    }
    Ok(())
}

#[allow(unsafe_code)]
fn dense_scores(matrix: &[f32], rows: usize, dimensions: usize, query: &[f32]) -> Vec<f32> {
    if rows == 0 {
        return Vec::new();
    }
    let mut scores = vec![0.0_f32; rows];
    // SAFETY: A is a contiguous rows×dimensions matrix, B has `dimensions`
    // elements, C has `rows` distinct elements, and every supplied stride stays
    // within those allocations. matrixmultiply selects its own checked CPU kernel.
    unsafe {
        matrixmultiply::sgemm(
            rows,
            dimensions,
            1,
            1.0,
            matrix.as_ptr(),
            dimensions as isize,
            1,
            query.as_ptr(),
            1,
            1,
            0.0,
            scores.as_mut_ptr(),
            1,
            1,
        );
    }
    scores
}

fn semantic_key(artifact: &SkillArtifact) -> String {
    let mut key = artifact.description.trim().to_lowercase();
    for export in &artifact.exports {
        key.push('\0');
        key.push_str(&export.name);
        key.push('\0');
        key.push_str(&export.signature);
    }
    key
}

fn manifest_size(artifact: &SkillArtifact) -> usize {
    artifact.id.len()
        + artifact.description.len()
        + artifact
            .exports
            .iter()
            .map(|export| export.name.len() + export.signature.len())
            .sum::<usize>()
        + artifact
            .capability
            .grants
            .iter()
            .map(|grant| serde_json::to_vec(grant).map_or(0, |bytes| bytes.len()))
            .sum::<usize>()
        + 128
}

pub(crate) fn lexical_tokens(text: &str) -> Vec<String> {
    text.split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty() && !LEXICAL_STOP_WORDS.contains(&term.as_str()))
        .take(MAX_QUERY_TERMS)
        .collect()
}

pub(crate) fn lexical_query_terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for term in query
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty() && !LEXICAL_STOP_WORDS.contains(&term.as_str()))
    {
        if seen.insert(term.clone()) {
            terms.push(term);
            if terms.len() == MAX_QUERY_TERMS {
                break;
            }
        }
    }
    terms.sort();
    terms
}

fn fts_query(connection: &Connection, query: &str) -> Result<Option<String>, rusqlite::Error> {
    let total_documents =
        connection.query_row("SELECT count(*) FROM snapshot_search", [], |row| {
            row.get::<_, i64>(0)
        })?;
    if total_documents == 0 {
        return Ok(None);
    }

    let mut statement =
        connection.prepare_cached("SELECT doc FROM snapshot_vocab WHERE term = ?1")?;
    let mut weighted = Vec::new();
    for term in lexical_query_terms(query) {
        let document_frequency = statement
            .query_row(params![term], |row| row.get::<_, i64>(0))
            .optional()?;
        let Some(document_frequency) = document_frequency else {
            continue;
        };
        if document_frequency == 0 {
            continue;
        }
        let numerator = total_documents.saturating_sub(document_frequency) as f64 + 0.5;
        let denominator = document_frequency as f64 + 0.5;
        let idf = (1.0 + numerator / denominator).ln();
        weighted.push((term, idf));
    }
    weighted.sort_by(|(left_term, left_idf), (right_term, right_idf)| {
        right_idf
            .total_cmp(left_idf)
            .then_with(|| left_term.cmp(right_term))
    });
    weighted.truncate(MAX_FTS_QUERY_TERMS);
    weighted.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok((!weighted.is_empty()).then(|| {
        weighted
            .into_iter()
            .map(|(term, _)| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ")
    }))
}
