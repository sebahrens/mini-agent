//! Opt-in `tool_choice` forcing for the first model request of a turn.
//!
//! An `@agent:<name>` mention turn under `agent_mention_tool_choice = "force"`
//! carries a typed marker on its [`PromptInput`](crate::agent::input::PromptInput)
//! naming the tool to force (the `task` tool). [`FirstRequestToolHook`] turns
//! that marker into a per-request `tool_choice` patch on the turn's first
//! completion call only. Once a model turn is accepted (the forced call has
//! been answered by the model), the hook disarms, so the request that carries
//! the tool result, and every later request in the same multi-step turn or in
//! a continuation, falls back to the agent's baseline (`auto`) and the model
//! can answer. A failed first attempt that the runner retries before any model
//! output is still the first request, so it stays armed until the model
//! responds (an accepted turn, a tool call or a rejected tool call).
//!
//! The hook never errors. The marker is dropped (with a debug log, falling
//! back to the routing hint alone) when the provider path cannot honour
//! `tool_choice` (decided by the caller before spawning) or when the tool is
//! not advertised in the agent's tool set for this request.

use rig::agent::{Agent, AgentHook, Flow, HookContext, RequestPatch, StepEvent, StepEventKind};
use rig::completion::CompletionModel;
use rig::message::ToolChoice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Name of the delegation tool a mention turn forces.
pub(crate) const TASK_TOOL_NAME: &str =
    <crate::extras::subagents::task_tool::TaskTool as rig::tool::Tool>::NAME;

/// Per-turn hook that forces `tool` on the first completion request only.
/// A disarmed hook (no marker, tool absent) is a no-op.
#[derive(Clone, Default)]
pub(crate) struct FirstRequestToolHook {
    tool: Option<&'static str>,
    armed: Arc<AtomicBool>,
}

impl FirstRequestToolHook {
    /// Arm for `tool` only when the agent actually advertises it; otherwise
    /// return a disarmed hook and log the hint fallback.
    pub(crate) async fn for_turn<M: CompletionModel>(
        agent: &Agent<M>,
        tool: Option<&'static str>,
    ) -> Self {
        let Some(tool) = tool else {
            return Self::default();
        };
        let advertised = match agent.tool_server_handle.get_tool_defs(None).await {
            Ok(defs) => defs.iter().any(|def| def.name == tool),
            Err(error) => {
                tracing::debug!(%error, "tool definitions unavailable; tool_choice not forced");
                false
            }
        };
        if !advertised {
            tracing::debug!(
                tool,
                "forced first-request tool is not in this request's tool list; using the routing hint only"
            );
            return Self::default();
        }
        Self {
            tool: Some(tool),
            armed: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl<M: CompletionModel> AgentHook<M> for FirstRequestToolHook {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        let Some(tool) = self.tool else {
            return Flow::Continue;
        };
        match event {
            StepEvent::CompletionCall { .. } if self.armed.load(Ordering::Acquire) => {
                Flow::patch_request(RequestPatch::new().tool_choice(ToolChoice::Specific {
                    function_names: vec![tool.to_owned()],
                }))
            }
            // Any model output for this request (an accepted turn, a tool
            // call, or a rejected call the runner may retry) ends forcing.
            StepEvent::ModelTurnFinished { .. }
            | StepEvent::ToolCall { .. }
            | StepEvent::InvalidToolCall(_) => {
                self.armed.store(false, Ordering::Release);
                Flow::Continue
            }
            _ => Flow::Continue,
        }
    }
    fn observes(&self, kind: StepEventKind) -> bool {
        self.tool.is_some()
            && matches!(
                kind,
                StepEventKind::CompletionCall
                    | StepEventKind::ModelTurnFinished
                    | StepEventKind::ToolCall
                    | StepEventKind::InvalidToolCall
            )
    }
}
