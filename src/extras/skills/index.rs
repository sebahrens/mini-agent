//! Exact in-memory metadata retrieval for Agent Skills progressive disclosure.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::extras::js::skills::embed::ModelMetadata;
use crate::extras::js::skills::index::{lexical_query_terms, lexical_tokens};

use super::catalog::ResourceMetadata;

#[derive(Debug, Clone)]
pub struct AgentSkillRecord {
    pub name: String,
    pub description: String,
    pub digest: String,
    pub tags: Vec<String>,
    pub identifiers: Vec<String>,
    pub skill_md_path: PathBuf,
    pub skill_md_bytes: u64,
    pub skill_md_sha256: String,
    pub resources: Vec<ResourceMetadata>,
    /// Non-authoritative display metadata only.
    pub allowed_tools: Option<String>,
    pub(crate) embedding: Vec<f32>,
}

impl AgentSkillRecord {
    pub fn embedding_document(&self) -> String {
        format!(
            "{}\nName: {}\nTags: {}\nIdentifiers: {}",
            self.description,
            self.name,
            self.tags.join(", "),
            self.identifiers.join(", ")
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentSkillSearchPolicy {
    pub max_skills: usize,
    pub score_floor: f32,
    pub metadata_byte_budget: usize,
    pub instruction_byte_budget: usize,
}

impl Default for AgentSkillSearchPolicy {
    fn default() -> Self {
        Self {
            max_skills: 3,
            score_floor: 0.20,
            metadata_byte_budget: 8 * 1024,
            instruction_byte_budget: 48 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScoredAgentSkill {
    pub record: Arc<AgentSkillRecord>,
    pub generation: u64,
    pub score: f32,
    pub rank: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentSkillIndexError {
    #[error("Agent Skill vector dimensions mismatch for {0}")]
    DimensionMismatch(String),
    #[error("Agent Skill vector contains a non-finite value for {0}")]
    NonFinite(String),
    #[error("query vector is incompatible with the Agent Skill catalog")]
    IncompatibleQuery,
}

/// Immutable catalog generation. Search performs no file access or embedding.
pub struct AgentSkillIndex {
    generation: u64,
    model: ModelMetadata,
    records: Arc<[Arc<AgentSkillRecord>]>,
}

impl AgentSkillIndex {
    pub fn build(
        generation: u64,
        model: ModelMetadata,
        mut records: Vec<AgentSkillRecord>,
    ) -> Result<Self, AgentSkillIndexError> {
        records.sort_by(|left, right| left.digest.cmp(&right.digest));
        let mut seen = HashSet::new();
        let records = records
            .into_iter()
            .filter(|record| seen.insert(record.digest.clone()))
            .map(|record| {
                if record.embedding.len() != model.dimensions {
                    return Err(AgentSkillIndexError::DimensionMismatch(record.digest));
                }
                if !record.embedding.iter().all(|value| value.is_finite()) {
                    return Err(AgentSkillIndexError::NonFinite(record.digest));
                }
                Ok(Arc::new(record))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            generation,
            model,
            records: records.into(),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn search(
        &self,
        query: &[f32],
        policy: &AgentSkillSearchPolicy,
    ) -> Result<Vec<ScoredAgentSkill>, AgentSkillIndexError> {
        if query.len() != self.model.dimensions || !query.iter().all(|value| value.is_finite()) {
            return Err(AgentSkillIndexError::IncompatibleQuery);
        }
        let mut ranked = self
            .records
            .iter()
            .filter_map(|record| {
                let score = query
                    .iter()
                    .zip(&record.embedding)
                    .map(|(left, right)| left * right)
                    .sum::<f32>();
                (score >= policy.score_floor).then_some((Arc::clone(record), score))
            })
            .collect::<Vec<_>>();
        ranked.sort_unstable_by(|(left, left_score), (right, right_score)| {
            right_score
                .total_cmp(left_score)
                .then_with(|| left.digest.cmp(&right.digest))
        });

        self.apply_budgets(ranked, policy)
    }

    /// Search discovery metadata without using embedding similarity.
    ///
    /// This is the production fallback for the deterministic hash embedder,
    /// whose vectors are stable but deliberately carry no semantic meaning.
    pub fn search_lexical(
        &self,
        query: &str,
        policy: &AgentSkillSearchPolicy,
    ) -> Result<Vec<ScoredAgentSkill>, AgentSkillIndexError> {
        const MAX_WEIGHTED_TERMS: usize = 16;
        const K1: f32 = 1.2;
        const B: f32 = 0.75;

        let query_terms = lexical_query_terms(query);
        if query_terms.is_empty() || self.records.is_empty() {
            return Ok(Vec::new());
        }
        let documents = self
            .records
            .iter()
            .map(|record| lexical_tokens(&record.embedding_document()))
            .collect::<Vec<_>>();
        let average_document_length =
            documents.iter().map(Vec::len).sum::<usize>().max(1) as f32 / documents.len() as f32;
        let corpus_size = documents.len() as f32;
        let mut weighted_terms = query_terms
            .into_iter()
            .filter_map(|term| {
                let document_frequency = documents
                    .iter()
                    .filter(|document| document.iter().any(|token| token == &term))
                    .count();
                if document_frequency == 0 {
                    return None;
                }
                let frequency = document_frequency as f32;
                let idf = (1.0 + (corpus_size - frequency + 0.5) / (frequency + 0.5)).ln();
                Some((term, idf))
            })
            .collect::<Vec<_>>();
        weighted_terms.sort_by(|(left_term, left_idf), (right_term, right_idf)| {
            right_idf
                .total_cmp(left_idf)
                .then_with(|| left_term.cmp(right_term))
        });
        weighted_terms.truncate(MAX_WEIGHTED_TERMS);

        let mut ranked = self
            .records
            .iter()
            .zip(documents)
            .filter_map(|(record, document)| {
                let document_length = document.len() as f32;
                let score = weighted_terms
                    .iter()
                    .map(|(term, idf)| {
                        let term_frequency =
                            document.iter().filter(|token| *token == term).count() as f32;
                        if term_frequency == 0.0 {
                            return 0.0;
                        }
                        let length_normalization =
                            K1 * (1.0 - B + B * document_length / average_document_length);
                        idf * (term_frequency * (K1 + 1.0))
                            / (term_frequency + length_normalization)
                    })
                    .sum::<f32>();
                (score > 0.0).then_some((Arc::clone(record), score))
            })
            .collect::<Vec<_>>();
        ranked.sort_unstable_by(|(left, left_score), (right, right_score)| {
            right_score
                .total_cmp(left_score)
                .then_with(|| left.digest.cmp(&right.digest))
        });

        self.apply_budgets(ranked, policy)
    }

    fn apply_budgets(
        &self,
        ranked: Vec<(Arc<AgentSkillRecord>, f32)>,
        policy: &AgentSkillSearchPolicy,
    ) -> Result<Vec<ScoredAgentSkill>, AgentSkillIndexError> {
        let mut metadata_bytes = 0usize;
        let mut instruction_bytes = 0usize;
        let mut selected = Vec::new();
        for (record, score) in ranked {
            if selected.len() >= policy.max_skills {
                break;
            }
            let next_metadata = metadata_bytes
                .saturating_add(record.name.len())
                .saturating_add(record.description.len())
                .saturating_add(record.digest.len());
            let instruction_len = usize::try_from(record.skill_md_bytes).unwrap_or(usize::MAX);
            let next_instructions = instruction_bytes.saturating_add(instruction_len);
            if next_metadata > policy.metadata_byte_budget
                || next_instructions > policy.instruction_byte_budget
            {
                continue;
            }
            metadata_bytes = next_metadata;
            instruction_bytes = next_instructions;
            selected.push(ScoredAgentSkill {
                record,
                generation: self.generation,
                score,
                rank: selected.len() + 1,
            });
        }
        Ok(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        name: &str,
        description: &str,
        digest: &str,
        embedding: Vec<f32>,
    ) -> AgentSkillRecord {
        AgentSkillRecord {
            name: name.to_string(),
            description: description.to_string(),
            digest: digest.to_string(),
            tags: vec![name.to_string()],
            identifiers: vec![name.to_string()],
            skill_md_path: PathBuf::from(format!("/{name}/SKILL.md")),
            skill_md_bytes: 128,
            skill_md_sha256: digest.to_string(),
            resources: Vec::new(),
            allowed_tools: None,
            embedding,
        }
    }

    #[test]
    fn lexical_search_selects_relevant_agent_skill_without_dense_similarity() {
        let model = ModelMetadata {
            model_id: "deterministic-hash".to_string(),
            model_revision: "deterministic-v2".to_string(),
            dimensions: 2,
            normalized: true,
        };
        let index = AgentSkillIndex::build(
            4,
            model,
            vec![
                record(
                    "json-tools",
                    "Parse JSON documents and extract object keys",
                    "aaa",
                    vec![0.0, 1.0],
                ),
                record(
                    "image-tools",
                    "Resize photographs and convert image formats",
                    "bbb",
                    vec![1.0, 0.0],
                ),
            ],
        )
        .unwrap();

        let selected = index
            .search_lexical(
                "please parse this JSON file and print the keys",
                &AgentSkillSearchPolicy::default(),
            )
            .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].record.digest, "aaa");
        assert!(selected[0].score > 0.0);
    }

    #[test]
    fn lexical_search_does_not_return_an_unscored_fallback() {
        let model = ModelMetadata {
            model_id: "deterministic-hash".to_string(),
            model_revision: "deterministic-v2".to_string(),
            dimensions: 2,
            normalized: true,
        };
        let index = AgentSkillIndex::build(
            1,
            model,
            vec![record(
                "json-tools",
                "Parse JSON documents",
                "aaa",
                vec![1.0, 0.0],
            )],
        )
        .unwrap();

        assert!(
            index
                .search_lexical("compose a symphony", &AgentSkillSearchPolicy::default())
                .unwrap()
                .is_empty()
        );
    }
}
