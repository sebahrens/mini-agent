//! Immutable active-only visibility snapshots.
//!
//! Phase 4 uses these small value objects to prove a durable canary remains
//! outside retrieval, prompt manifests, frozen turn bundles, and JS source
//! execution. Phase 5 may add bounded canary routing, but must do so explicitly.

use std::collections::BTreeMap;

use super::store::{SkillStore, StoreError};
use super::{SkillArtifact, SkillExport};

#[derive(Debug, Clone)]
pub(crate) struct SkillIndex {
    active: BTreeMap<String, SkillArtifact>,
}

impl SkillIndex {
    pub(crate) fn load(store: &SkillStore) -> Result<Self, StoreError> {
        let active = store
            .list_retrievable()?
            .into_iter()
            .map(|artifact| (artifact.id.clone(), artifact))
            .collect();
        Ok(Self { active })
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.active.contains_key(id)
    }

    pub(crate) fn manifest(&self) -> PromptSkillManifest {
        PromptSkillManifest {
            entries: self
                .active
                .values()
                .map(|artifact| PromptSkillEntry {
                    id: artifact.id.clone(),
                    description: artifact.description.clone(),
                    exports: artifact.exports.clone(),
                })
                .collect(),
        }
    }

    pub(crate) fn freeze(&self, ids: &[String]) -> TurnSkillBundle {
        TurnSkillBundle {
            artifacts: ids
                .iter()
                .filter_map(|id| self.active.get(id).cloned())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptSkillEntry {
    pub id: String,
    pub description: String,
    pub exports: Vec<SkillExport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptSkillManifest {
    pub entries: Vec<PromptSkillEntry>,
}

impl PromptSkillManifest {
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnSkillBundle {
    pub artifacts: Vec<SkillArtifact>,
}

impl TurnSkillBundle {
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.artifacts.iter().any(|artifact| artifact.id == id)
    }

    pub(crate) fn js_source(&self) -> String {
        self.artifacts
            .iter()
            .map(|artifact| artifact.source.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
