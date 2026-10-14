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
        let output = serde_json::to_string(
            &serde_json::json!({"agent_skills":candidate.selection.selected,"learned_js":[]}),
        )
        .map_err(|_| ToolError::Msg("skills_search encoding failed".into()))?;
        if output.len() > 8192 {
            return Err(ToolError::Msg(
                "skills_search encoded metadata budget exceeded".into(),
            ));
        }
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
        let output = serde_json::to_string(
            &serde_json::json!({"agent_skills":selection.selected,"learned_js":[]}),
        )
        .map_err(|_| ToolError::Msg("skills_search encoding failed".into()))?;
        if output.len() > 8192 {
            return Err(ToolError::Msg(
                "skills_search encoded metadata budget exceeded".into(),
            ));
        }
        Ok(output)
    }
}
