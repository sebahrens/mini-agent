//! Grouped TUI state. The `App` used to carry ~40 flat fields and pass 10-20
//! of them into every helper; these structs group that state by lifetime and
//! purpose so helpers take a handful of coherent bundles instead.

use std::collections::VecDeque;
use std::sync::Arc;

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
    pub workspace: Arc<crate::paths::WorkspaceBinding>,
    pub client: AnyClient,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub sandbox: Sandbox,
    #[cfg(feature = "skills")]
    pub skill_services: Arc<crate::extras::js::skills::session::SkillServiceOwner>,
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
            workspace: &self.workspace,
            client: &self.client,
            permission: &self.permission,
            ask_tx: &self.ask_tx,
            sandbox: &self.sandbox,
            read_tracker: &self.session.read_tracker,
            #[cfg(feature = "skills")]
            skill_services: &self.skill_services,
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
        workspace: Arc<crate::paths::WorkspaceBinding>,
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
            workspace,
            client,
            permission,
            ask_tx,
            sandbox,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
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
    pub workspace: &'a Arc<crate::paths::WorkspaceBinding>,
    pub client: &'a AnyClient,
    pub permission: &'a Option<PermCheck>,
    pub ask_tx: &'a Option<AskSender>,
    pub sandbox: &'a Sandbox,
    pub read_tracker: &'a crate::agent::tools::ReadTracker,
    #[cfg(feature = "skills")]
    pub skill_services: &'a Arc<crate::extras::js::skills::session::SkillServiceOwner>,
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
        crate::provider::build_agent_in_workspace(
            model,
            self.cli,
            self.cfg,
            self.context,
            self.workspace.clone(),
            self.permission.clone(),
            self.ask_tx.clone(),
            self.sandbox.clone(),
            self.read_tracker.clone(),
            reasoning_enabled,
            temperature,
            extra_body,
            #[cfg(feature = "skills")]
            self.skill_services.clone(),
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

/// Transaction snapshot for one user-authored main turn. No in-progress turn
/// is authoritative on disk: terminal transitions commit any observed agent
/// progress, while a zero-progress failure restores the pre-turn transcript.
/// Independently authoritative usage and explicit permission grants survive
/// either transition.
pub(crate) struct PendingMainTurn {
    prompt: String,
    session_id: compact_str::CompactString,
    session_before: SessionRollback,
    started_at: Option<compact_str::CompactString>,
    tool_output_paths: Vec<std::path::PathBuf>,
    #[cfg(feature = "memory")]
    memory_summaries: Vec<(String, Option<usize>)>,
}

struct SessionRollback {
    messages: Vec<crate::session::SessionMessage>,
    compactions: Vec<crate::session::Compaction>,
    updated_at: compact_str::CompactString,
    total_estimated_tokens: u64,
    calibrated_tokens: u64,
    calibrated_msg_count: usize,
    rewind_undo: Option<crate::session::RewindUndo>,
    #[cfg(feature = "multimodal")]
    pending_media: Vec<crate::extras::multimodal::MediaAttachment>,
}

impl PendingMainTurn {
    pub(crate) fn capture(session: &Session, prompt: &str) -> Self {
        Self {
            prompt: prompt.to_string(),
            session_id: session.id.clone(),
            session_before: SessionRollback {
                messages: session.messages.clone(),
                compactions: session.compactions.clone(),
                updated_at: session.updated_at.clone(),
                total_estimated_tokens: session.total_estimated_tokens,
                calibrated_tokens: session.calibrated_tokens,
                calibrated_msg_count: session.calibrated_msg_count,
                rewind_undo: session.rewind_undo.clone(),
                #[cfg(feature = "multimodal")]
                pending_media: Vec::new(),
            },
            started_at: None,
            tool_output_paths: Vec::new(),
            #[cfg(feature = "memory")]
            memory_summaries: Vec::new(),
        }
    }

    pub(crate) fn prompt(&self) -> &str {
        &self.prompt
    }

    pub(crate) fn record_started(&mut self, timestamp: compact_str::CompactString) {
        self.started_at = Some(timestamp);
    }

    pub(crate) fn record_tool_output(&mut self, path: std::path::PathBuf) {
        self.tool_output_paths.push(path);
    }

    pub(crate) fn has_progress(
        &self,
        session: &Session,
        response_buf: &str,
        turn_trace: &[compact_str::CompactString],
    ) -> bool {
        !response_buf.trim().is_empty()
            || !turn_trace.is_empty()
            || session.messages.len() > self.session_before.messages.len().saturating_add(1)
            || session.compactions.len() != self.session_before.compactions.len()
            || {
                #[cfg(feature = "memory")]
                {
                    !self.memory_summaries.is_empty()
                }
                #[cfg(not(feature = "memory"))]
                {
                    false
                }
            }
    }

    pub(crate) fn has_recorded_turn_messages(&self, session: &Session) -> bool {
        session.messages.len() > self.session_before.messages.len().saturating_add(1)
    }

    pub(crate) fn finalize_unresolved_tool_calls(&self, session: &mut Session) {
        let turn_start = self
            .session_before
            .messages
            .len()
            .min(session.messages.len());
        let resolved: std::collections::HashSet<String> = session.messages[turn_start..]
            .iter()
            .filter(|message| message.role == crate::session::MessageRole::ToolResult)
            .filter_map(|message| message.tool_call_id.as_deref().map(str::to_owned))
            .collect();
        let mut unresolved = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in session.messages[turn_start..]
            .iter()
            .filter(|message| message.role == crate::session::MessageRole::ToolCall)
            .filter_map(|message| message.tool_call_id.as_deref())
        {
            if !resolved.contains(id) && seen.insert(id.to_owned()) {
                unresolved.push(id.to_owned());
            }
        }

        for id in unresolved {
            session.add_tool_result_with_id(
                &id,
                "unknown",
                crate::agent::runner::UNKNOWN_TOOL_OUTCOME,
            );
        }
    }

    #[cfg(feature = "multimodal")]
    pub(crate) fn take_pending_media(
        &mut self,
        session: &mut Session,
    ) -> &[crate::extras::multimodal::MediaAttachment] {
        self.session_before.pending_media = session.drain_media();
        &self.session_before.pending_media
    }

    #[cfg(feature = "memory")]
    pub(crate) fn stage_memory_summary(&mut self, summary: String, count: Option<usize>) {
        self.memory_summaries.push((summary, count));
    }

    pub(crate) fn commit_side_effects(self, persist_chat_history: bool) -> Vec<anyhow::Error> {
        let mut errors = Vec::new();
        if persist_chat_history && let Some(timestamp) = self.started_at {
            let entry = crate::session::chat_history::ChatHistoryEntry {
                content: self.prompt,
                timestamp,
            };
            if let Err(error) = crate::session::chat_history::append_entry(&entry) {
                errors.push(error);
            }
        }
        #[cfg(feature = "memory")]
        for (summary, count) in self.memory_summaries {
            crate::extras::memory::flush_compaction_summary(
                &crate::extras::memory::Mem::open(),
                &summary,
                count,
            );
        }
        errors
    }

    pub(crate) fn rollback(self, session: &mut Session) -> String {
        // Provider usage was actually incurred and an explicit AllowAlways
        // decision remains authoritative even when the transcript fails. Keep
        // those orthogonal durable facts while restoring every turn-owned
        // message, compaction, calibration, estimate, and rewind field.
        for path in &self.tool_output_paths {
            if let Err(error) =
                crate::session::storage::delete_uncommitted_tool_output(&self.session_id, path)
            {
                tracing::warn!("failed to remove uncommitted tool output: {error}");
            }
        }
        session.messages = self.session_before.messages;
        session.compactions = self.session_before.compactions;
        session.updated_at = self.session_before.updated_at;
        session.total_estimated_tokens = self.session_before.total_estimated_tokens;
        session.calibrated_tokens = self.session_before.calibrated_tokens;
        session.calibrated_msg_count = self.session_before.calibrated_msg_count;
        session.rewind_undo = self.session_before.rewind_undo;
        #[cfg(feature = "multimodal")]
        {
            session.pending_media = self.session_before.pending_media;
        }
        self.prompt
    }
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
    pub pending_turn: Option<PendingMainTurn>,
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

#[derive(Default)]
pub(crate) struct ChainState {
    pub pending: Option<crate::extras::chain::ChainPhase>,
    pub label_msg: Option<String>,
    pub dot_prompt_restore: Option<String>,
    pub loop_label: Option<String>,
    #[cfg(feature = "loop")]
    pub loop_state: Option<crate::extras::r#loop::LoopState>,
}

/// User-facing feature toggles owned by slash commands.
pub(crate) struct SlashState {
    pub show_reasoning: bool,
    pub reasoning_enabled: bool,
    pub todo_tools_enabled: bool,
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
