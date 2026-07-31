//! Exact in-memory metadata retrieval for Agent Skills progressive disclosure.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::extras::js::skills::embed::ModelMetadata;

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
