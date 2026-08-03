//! Grouped TUI state. The `App` used to carry ~40 flat fields and pass 10-20
//! of them into every helper; these structs group that state by lifetime and
//! purpose so helpers take a handful of coherent bundles instead.

use std::collections::VecDeque;

use tokio::sync::mpsc;

use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
use crate::event::AgentEvent;
#[cfg(feature = "loop")]
use crate::event::ValidationOperationId;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::extras::status_signals::StatusSignals;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::{AnyAgent, AnyClient};
use crate::sandbox::Sandbox;
use crate::session::Session;

/// Shared resources every part of the TUI reaches for: static config, the
/// session, context files, the provider client, and the capability handles
/// needed to (re)build agents.
pub(crate) struct UiContext<'a> {
    pub cli: &'a Cli,
    pub cfg: &'a Config,
    pub session: &'a mut Session,
    pub context: &'a mut ContextFiles,
    pub client: AnyClient,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub sandbox: Sandbox,
    pub status_signals: Option<StatusSignals>,
    #[cfg(feature = "mcp")]
    pub mcp_manager: Option<McpClientManager>,
}

impl<'a> UiContext<'a> {
    /// Borrow the pieces [`AgentBuildCtx::rebuild_agent`] needs.
    pub(crate) fn agent_build_ctx(&self) -> AgentBuildCtx<'_> {
        AgentBuildCtx {
            cli: self.cli,
            cfg: self.cfg,
            context: self.context,
            client: &self.client,
            permission: &self.permission,
            ask_tx: &self.ask_tx,
            sandbox: &self.sandbox,
            read_tracker: &self.session.read_tracker,
            #[cfg(feature = "mcp")]
            mcp_manager: self.mcp_manager.as_ref(),
        }
    }

    /// Composition root: built once in `main` and threaded through the TUI.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cli: &'a Cli,
        cfg: &'a Config,
        session: &'a mut Session,
        context: &'a mut ContextFiles,
        client: AnyClient,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        sandbox: Sandbox,
        status_signals: Option<StatusSignals>,
    ) -> Self {
        Self {
            cli,
            cfg,
            session,
            context,
            client,
            permission,
            ask_tx,
            sandbox,
            status_signals,
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        }
    }
}

/// Everything needed to (re)build the main agent, borrowed from whichever
/// state bundle the caller has: [`UiContext`] in the main loop and mid-turn
/// compaction, `SlashCtx` in slash commands, or owned clones in the startup
/// prebuild task. Centralizes the per-model resolution (completion model,
/// temperature, extra_body) and the `build_agent` call itself so every
/// rebuild path stays in sync.
pub(crate) struct AgentBuildCtx<'a> {
    pub cli: &'a Cli,
    pub cfg: &'a Config,
    pub context: &'a ContextFiles,
    pub client: &'a AnyClient,
    pub permission: &'a Option<PermCheck>,
    pub ask_tx: &'a Option<AskSender>,
    pub sandbox: &'a Sandbox,
    pub read_tracker: &'a crate::agent::tools::ReadTracker,
    #[cfg(feature = "mcp")]
    pub mcp_manager: Option<&'a McpClientManager>,
}

impl AgentBuildCtx<'_> {
    /// Build the main agent for `model_id` (usually `session.model`; model
    /// switches pass the not-yet-committed new id).
    pub(crate) async fn rebuild_agent(&self, model_id: &str, reasoning_enabled: bool) -> AnyAgent {
        let model = self.client.completion_model(model_id.to_string());
        let temperature = crate::config::resolve_temperature(self.cli, self.cfg, model_id);
        let extra_body = crate::config::resolve_extra_body(self.cfg, model_id);
        crate::provider::build_agent(
            model,
            self.cli,
            self.cfg,
            self.context,
            self.permission.clone(),
            self.ask_tx.clone(),
            self.sandbox.clone(),
            self.read_tracker.clone(),
            reasoning_enabled,
            temperature,
            extra_body,
            #[cfg(feature = "mcp")]
            self.mcp_manager,
        )
        .await
    }
}

/// Transient state of the main agent run: the agent handle, its event
/// stream and abort handle, queued user input, and streaming-response scratch.
#[cfg(feature = "loop")]
pub(super) struct ActiveValidation {
    id: ValidationOperationId,
    cancellation: crate::extras::r#loop::validation::ValidationCancellation,
}

#[derive(Default)]
pub(crate) struct AgentRunState {
    pub agent: Option<AnyAgent>,
    pub is_running: bool,
    pub agent_rx: Option<mpsc::Receiver<AgentEvent>>,
    pub main_abort: Option<tokio::task::AbortHandle>,
    #[cfg(feature = "loop")]
    pub(super) active_validation: Option<ActiveValidation>,
    #[cfg(feature = "loop")]
    pub(super) validation_generation: u64,
    pub pending_inputs: VecDeque<String>,
    pub agent_line_started: bool,
    pub response_buf: String,
    pub response_start_block: Option<usize>,
    pub pending_send: Option<String>,
    pub was_reasoning: bool,
    pub turn_trace: Vec<compact_str::CompactString>,
    pub awaiting_compaction_relief: bool,
}

#[cfg(feature = "loop")]
impl AgentRunState {
    pub(crate) fn begin_validation(
        &mut self,
        cancellation: crate::extras::r#loop::validation::ValidationCancellation,
    ) -> ValidationOperationId {
        self.validation_generation = self
            .validation_generation
            .checked_add(1)
            .expect("validation operation generation exhausted");
        let id = ValidationOperationId(self.validation_generation);
        self.active_validation = Some(ActiveValidation { id, cancellation });
        id
    }

    pub(crate) fn validation_active(&self) -> bool {
        self.active_validation.is_some()
    }

    /// Retires the current generation before signalling its worker. Any
    /// completion already queued for that generation is stale immediately.
    pub(crate) fn cancel_validation(&mut self) -> bool {
        let Some(active) = self.active_validation.take() else {
            return false;
        };
        active.cancellation.cancel();
        true
    }

    /// Accepts exactly the operation currently registered in this run state.
    /// A stale completion must not clear or replace a newer validation.
    pub(crate) fn complete_validation(&mut self, id: ValidationOperationId) -> bool {
        if self.active_validation.as_ref().map(|active| active.id) != Some(id) {
            return false;
        }
        self.active_validation = None;
        true
    }
}

/// What happens when the current run finishes: chained prompts, dot-prompt
/// restore, /loop iterations, and worktree-merge returns.
#[derive(Default)]
pub(crate) struct ChainState {
    pub pending: Option<crate::extras::chain::ChainPhase>,
    pub label_msg: Option<String>,
    pub dot_prompt_restore: Option<String>,
    pub loop_label: Option<String>,
    #[cfg(feature = "loop")]
    pub loop_state: Option<crate::extras::r#loop::LoopState>,
    #[cfg(feature = "git-worktree")]
    pub wt_return_path: Option<(String, String, String, bool)>,
}

/// User-facing feature toggles owned by slash commands.
pub(crate) struct SlashState {
    pub show_reasoning: bool,
    pub reasoning_enabled: bool,
    pub todo_tools_enabled: bool,
}

/// Provider-reported token usage for one finished turn.
#[derive(Clone, Copy, Default)]
pub(crate) struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

/// /btw aggregate stats shown in the statusline.
#[derive(Clone, Copy, Default)]
pub(crate) struct BtwStats {
    pub cost: f64,
    pub input: u64,
    pub output: u64,
}

#[cfg(all(test, feature = "loop"))]
mod validation_generation_tests {
    use super::*;

    fn cancellation() -> crate::extras::r#loop::validation::ValidationCancellation {
        crate::extras::r#loop::validation::start(&Sandbox::new(false, "bwrap"), "true")
            .cancellation()
    }

    #[test]
    fn cancelled_validation_then_normal_run_rejects_stale_completion() {
        let mut run = AgentRunState::default();
        let stale = run.begin_validation(cancellation());
        assert!(run.cancel_validation());

        // A normal run may start before the cancelled worker finishes cleanup.
        run.is_running = true;
        assert!(!run.complete_validation(stale));
        assert!(run.is_running);
        assert!(!run.validation_active());
    }

    #[test]
    fn cancelled_validation_then_new_loop_preserves_new_generation() {
        let mut run = AgentRunState::default();
        let stale = run.begin_validation(cancellation());
        assert!(run.cancel_validation());
        let current = run.begin_validation(cancellation());

        assert_ne!(stale, current);
        assert!(!run.complete_validation(stale));
        assert!(run.validation_active());
        assert!(run.complete_validation(current));
        assert!(!run.validation_active());
    }

    #[test]
    fn current_validation_completion_retires_active_generation() {
        let mut run = AgentRunState::default();
        let current = run.begin_validation(cancellation());

        assert!(run.complete_validation(current));
        assert!(!run.validation_active());
        assert!(!run.complete_validation(current));
    }
}

/// Parameters for a worktree merge-and-return run.
#[cfg(feature = "git-worktree")]
pub(crate) struct MergeRequest<'a> {
    pub branch: &'a str,
    pub target: &'a str,
    pub main_path: &'a str,
    pub wt_path: &'a str,
    pub force: bool,
}
