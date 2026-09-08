//! On-demand, read-only skill discovery at an agent tool-result boundary.

use std::sync::Arc;

use rig::tool::Tool;
use serde::{Deserialize, Serialize};

use super::session::SkillSessionServices;
use crate::agent::tools::ToolError;

const MAX_SEARCH_QUERY_BYTES: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
pub struct SkillsSearchArgs {
    pub query: String,
}

pub struct SkillsSearchTool {
    services: Arc<SkillSessionServices>,
}

impl SkillsSearchTool {
    pub(crate) fn new(services: Arc<SkillSessionServices>) -> Self {
        Self { services }
    }
}

#[derive(Serialize)]
struct LearnedSkillSummary<'a> {
    id: &'a str,
    description: &'a str,
    exports: &'a [super::SkillExport],
}

#[derive(Serialize)]
struct SearchResult<'a> {
    agent_skills: &'a [super::turn::ResolvedAgentSkill],
    learned_js: Vec<LearnedSkillSummary<'a>>,
}

impl Tool for SkillsSearchTool {
    const NAME: &'static str = "skills_search";
    type Error = ToolError;
    type Args = SkillsSearchArgs;
    type Output = String;

    fn description(&self) -> String {
        "Search installed skills using a model-generated query. On a default build this matches words, not meaning: the query is reduced to its most distinctive terms and any of them may match, so name the concrete nouns you expect in a skill's description rather than paraphrasing the goal. Returns bounded discovery metadata: Agent Skill names/descriptions/digests and learned-JS IDs/descriptions/export signatures. The selected learned-JS bundle is re-frozen for subsequent tool calls. Call this tool by itself and wait for its result before invoking a newly discovered JS export. It is read-only and cannot approve, activate, install, or widen a skill's capabilities."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_SEARCH_QUERY_BYTES,
                    "description": "A focused description of the capability needed now."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: SkillsSearchArgs) -> Result<String, ToolError> {
        let query = validate_query(&args.query)?;

        let discovery = self
            .services
            .search(query)
            .await
            .map_err(|error| ToolError::Msg(format!("skills_search: {error}")))?;
        if !discovery.diagnostics.is_empty() {
            tracing::debug!(
                diagnostic_count = discovery.diagnostics.len(),
                "skills_search completed with internal diagnostics"
            );
        }
        let learned_js = discovery
            .learned_js
            .skills
            .iter()
            .map(|skill| LearnedSkillSummary {
                id: &skill.id,
                description: &skill.description,
                exports: &skill.exports,
            })
            .collect();
        serde_json::to_string(&SearchResult {
            agent_skills: &discovery.agent_skills,
            learned_js,
        })
        .map_err(|error| ToolError::Msg(format!("skills_search: serialization failed: {error}")))
    }
}

fn validate_query(query: &str) -> Result<&str, ToolError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(ToolError::Msg(
            "skills_search: query must not be empty".to_string(),
        ));
    }
    if query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(ToolError::Msg(format!(
            "skills_search: query exceeds {MAX_SEARCH_QUERY_BYTES} bytes"
        )));
    }
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_shape_exposes_metadata_without_executable_source() {
        let agent_skills = vec![super::super::turn::ResolvedAgentSkill {
            name: "reviewer".to_string(),
            description: "Review code".to_string(),
            digest: "a".repeat(64),
        }];
        let exports = vec![super::super::SkillExport {
            name: "review".to_string(),
            signature: "(text: string) => object".to_string(),
        }];
        let learned_js = vec![LearnedSkillSummary {
            id: "b",
            description: "Review structured text",
            exports: &exports,
        }];
        let encoded = serde_json::to_string(&SearchResult {
            agent_skills: &agent_skills,
            learned_js,
        })
        .unwrap();

        assert!(encoded.contains("(text: string) => object"));
        assert!(!encoded.contains("source"));
        assert!(!encoded.contains("tests"));
        assert!(!encoded.contains("capability"));
    }

    #[test]
    fn query_validation_is_nonempty_and_byte_bounded() {
        assert!(validate_query(" \n\t ").is_err());
        assert_eq!(validate_query("  focused task  ").unwrap(), "focused task");
        assert!(validate_query(&"x".repeat(MAX_SEARCH_QUERY_BYTES + 1)).is_err());
        assert!(validate_query(&"é".repeat(MAX_SEARCH_QUERY_BYTES / 2 + 1)).is_err());
    }

    #[tokio::test]
    async fn search_call_refreezes_the_bundle_without_starting_a_new_turn() {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-skills-search-refreeze-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let paths = crate::paths::AppPaths::resolve(&crate::paths::PathEnvironment {
            platform: if cfg!(target_os = "macos") {
                crate::paths::PathPlatform::MacOs
            } else if cfg!(target_os = "windows") {
                crate::paths::PathPlatform::Windows
            } else {
                crate::paths::PathPlatform::Linux
            },
            home_dir: None,
            config_base: Some(root.join("config")),
            data_base: Some(root.join("data")),
            local_data_base: Some(root.join("local")),
            state_base: Some(root.join("state")),
            cache_base: Some(root.join("cache")),
            workspace_root: None,
            overrides: Default::default(),
        })
        .unwrap();
        let services = SkillSessionServices::for_test(&paths);
        let before = services.turn_context().snapshot();
        let tool = SkillsSearchTool::new(Arc::clone(&services));

        let result = tool
            .call(SkillsSearchArgs {
                query: "structured document review".to_string(),
            })
            .await
            .unwrap();
        let after = services.turn_context().snapshot();

        assert_eq!(
            before.turn_id, after.turn_id,
            "a mid-turn search must not re-draw the turn's canary route or \
             orphan earlier invocations from its outcome evidence"
        );
        assert_ne!(before.query_fingerprint, after.query_fingerprint);
        assert!(result.contains("agent_skills"));
        assert!(result.contains("learned_js"));
        assert!(!result.contains("diagnostics"));
        let _ = std::fs::remove_dir_all(root);
    }
}
