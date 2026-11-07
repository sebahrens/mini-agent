//! Instruction-only metadata search. It neither executes resources nor admits
//! learned JavaScript; the normal tool concurrency/permission wrappers remain.
use super::session::InstructionServices;
use crate::agent::tools::ToolError;
use rig::tool::Tool;
use serde::Deserialize;
use std::sync::Arc;

pub(crate) struct SkillsSearchTool {
    services: Arc<InstructionServices>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    query: String,
}
impl SkillsSearchTool {
    pub(crate) fn new(services: Arc<InstructionServices>) -> Self {
        Self { services }
    }
}
impl Tool for SkillsSearchTool {
    const NAME: &'static str = "skills_search";
    type Error = ToolError;
    type Args = Args;
    type Output = String;
    fn description(&self) -> String {
        "Search currently authorized instruction skills by concrete words. Returns bounded name, description and digest metadata. Resources stay inert; this tool cannot grant permissions, execute scripts, approve or activate learned skills.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"query":{"type":"string","minLength":1,"maxLength":8192}},"required":["query"],"additionalProperties":false})
    }
    async fn call_structured(
        &self,
        args: Args,
        extensions: &rig::tool::ToolCallExtensions,
    ) -> Result<rig::tool::ToolReturn<String>, ToolError> {
        let query = args.query.trim();
        if query.is_empty() || query.len() > 8192 {
            return Err(ToolError::Msg(
                "skills_search requires 1..8192 query bytes".into(),
            ));
        }
        let carrier = extensions
            .get::<rig::agent::hook::Scratchpad>()
            .and_then(|pad| pad.get::<super::session::InstructionCarrier>());
        let Some(carrier) = carrier else {
            return self.call(args).await.map(rig::tool::ToolReturn::success);
        };
        if !self.services.same_authority(&carrier.0.services) {
            return Err(ToolError::Msg(
                "skills_search invocation has a different captured source authority".into(),
            ));
        }
        let candidate = carrier
            .0
            .candidate(
                query,
                #[cfg(feature = "skills")]
                None,
            )
            .await
            .map_err(|_| ToolError::Msg("skills_search source or pin unavailable".into()))?;
        let output = encode_selected(&candidate.selection.selected)?;
        let output = carrier
            .0
            .native_result(
                output,
                super::selection::render_plain(&candidate.selection).map_err(ToolError::Msg)?,
            )
            .map_err(ToolError::Msg)?;
        Ok(
            rig::tool::ToolReturn::success(output.clone()).with_extension(
                super::session::SearchCandidate {
                    turn: carrier.0,
                    prepared: candidate,
                    output,
                    #[cfg(feature = "skills")]
                    learned: None,
                },
            ),
        )
    }
    async fn call(&self, args: Args) -> Result<String, ToolError> {
        let query = args.query.trim();
        if query.is_empty() || query.len() > 8192 {
            return Err(ToolError::Msg(
                "skills_search requires 1..8192 query bytes".into(),
            ));
        }
        let selection = self
            .services
            .select(query, None)
            .await
            .map_err(|_| ToolError::Msg("skills_search catalogue unavailable".into()))?;
        encode_selected(&selection.selected)
    }
}

/// Byte budget of the encoded `skills_search` metadata.
const METADATA_BUDGET: usize = 8192;

/// Encodes the selected skills within [`METADATA_BUDGET`]. When they do not
/// all fit the lowest-ranked entries are omitted and the result says how many
/// and how to see them (a narrower query), instead of failing the call.
fn encode_selected<T: serde::Serialize>(selected: &[T]) -> Result<String, ToolError> {
    let encode = |kept: usize| -> Result<String, ToolError> {
        let omitted = selected.len() - kept;
        let mut value = serde_json::json!({"agent_skills":&selected[..kept],"learned_js":[]});
        if omitted > 0 {
            value["omitted"] = serde_json::json!(omitted);
            value["note"] = serde_json::json!(format!(
                "{omitted} more matching skill(s) omitted to fit the {METADATA_BUDGET}-byte \
                 metadata budget; repeat skills_search with a narrower query to see them"
            ));
        }
        serde_json::to_string(&value)
            .map_err(|_| ToolError::Msg("skills_search encoding failed".into()))
    };
    for kept in (0..=selected.len()).rev() {
        let output = encode(kept)?;
        if output.len() <= METADATA_BUDGET {
            return Ok(output);
        }
    }
    Err(ToolError::Msg(
        "skills_search encoded metadata budget exceeded".into(),
    ))
}

#[cfg(test)]
mod encode_tests {
    use super::*;

    #[test]
    fn oversized_selection_is_trimmed_with_an_omitted_marker() {
        let entry = |index: usize| serde_json::json!({"name":format!("skill-{index}"),"description":"d".repeat(1024)});
        let selected: Vec<_> = (0..16).map(entry).collect();
        let output = encode_selected(&selected).unwrap();
        assert!(output.len() <= METADATA_BUDGET);
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        let kept = value["agent_skills"].as_array().unwrap().len();
        assert!(kept >= 1 && kept < 16, "{kept}");
        assert_eq!(value["omitted"], 16 - kept);
        assert!(value["note"].as_str().unwrap().contains("narrower query"));
        // The highest-ranked entries are the ones kept, in order.
        assert_eq!(value["agent_skills"][0]["name"], "skill-0");
    }

    #[test]
    fn small_selection_is_unchanged_and_has_no_marker() {
        let selected = vec![serde_json::json!({"name":"a","description":"b"})];
        let value: serde_json::Value =
            serde_json::from_str(&encode_selected(&selected).unwrap()).unwrap();
        assert!(value.get("omitted").is_none());
        assert_eq!(value["agent_skills"].as_array().unwrap().len(), 1);
    }
}
