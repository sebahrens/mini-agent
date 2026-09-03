pub(crate) mod add;
mod content;
mod features;
mod help;
#[cfg(feature = "hooks")]
mod hooks;
pub(crate) mod init;
mod memory;
#[cfg(feature = "memory")]
pub(crate) use memory::edit_memory_file;
#[cfg(unix)]
pub(crate) use memory::verify_memory_editor_preservation;
mod providers;
pub(crate) mod review;
mod session;
pub(crate) use session::is_persistence_restart_required;
pub(crate) mod settings;

pub(crate) use providers::warm_model_cache;

use smallvec::SmallVec;

use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::{AnyAgent, AnyClient};
use crate::sandbox::Sandbox;
use crate::session::{MessageRole, Session};
use crate::ui::events::render_session;
use crate::ui::input::InputEditor;
use crate::ui::renderer::Renderer;
use crate::ui::state::{AgentBuildCtx, AgentRunState, ChainState, SlashState, UiContext};
use crate::ui::terminal::TerminalGuard;

pub(crate) const C_AGENT: crossterm::style::Color = crossterm::style::Color::White;
pub(crate) const C_RESULT: crossterm::style::Color = crossterm::style::Color::DarkGrey;
pub(crate) const C_ERROR: crossterm::style::Color = crossterm::style::Color::Red;

pub struct SlashCtx<'a> {
    pub agent: &'a mut Option<AnyAgent>,
    pub client: &'a mut AnyClient,
    pub renderer: &'a mut Renderer,
    pub session: &'a mut Session,
    pub cli: &'a Cli,
    pub cfg: &'a Config,
    pub context: &'a mut ContextFiles,
    pub workspace: &'a std::sync::Arc<crate::paths::WorkspaceBinding>,
    pub show_reasoning: &'a mut bool,
    pub reasoning_enabled: &'a mut bool,
    pub is_running: &'a mut bool,
    pub input: &'a mut InputEditor,
    pub permission: &'a Option<PermCheck>,
    pub ask_tx: &'a Option<AskSender>,
    pub todo_tools_enabled: &'a mut bool,
    pub sandbox: &'a Sandbox,
    #[cfg(feature = "skills")]
    pub skill_services: &'a std::sync::Arc<crate::extras::js::skills::session::SkillServiceOwner>,
    pub terminal_guard: &'a mut TerminalGuard,
    #[cfg(feature = "loop")]
    pub loop_state: &'a mut Option<crate::extras::r#loop::LoopState>,
    #[cfg(feature = "mcp")]
    pub mcp_manager: Option<&'a crate::extras::mcp::McpClientManager>,
}

impl SlashCtx<'_> {
    /// Borrow the pieces [`AgentBuildCtx::rebuild_agent`] needs.
    fn agent_build_ctx(&self) -> AgentBuildCtx<'_> {
        AgentBuildCtx {
            cli: self.cli,
            cfg: self.cfg,
            context: self.context,
            workspace: self.workspace,
            client: self.client,
            permission: self.permission,
            ask_tx: self.ask_tx,
            sandbox: self.sandbox,
            read_tracker: &self.session.read_tracker,
            #[cfg(feature = "skills")]
            skill_services: self.skill_services,
            #[cfg(feature = "mcp")]
            mcp_manager: self.mcp_manager,
        }
    }

    async fn build_agent_for_client(
        &self,
        client: &AnyClient,
        model_id: &str,
        read_tracker: &crate::agent::tools::ReadTracker,
    ) -> AnyAgent {
        AgentBuildCtx {
            cli: self.cli,
            cfg: self.cfg,
            context: self.context,
            workspace: self.workspace,
            client,
            permission: self.permission,
            ask_tx: self.ask_tx,
            sandbox: self.sandbox,
            read_tracker,
            #[cfg(feature = "skills")]
            skill_services: self.skill_services,
            #[cfg(feature = "mcp")]
            mcp_manager: self.mcp_manager,
        }
        .rebuild_agent(model_id, *self.reasoning_enabled)
        .await
    }

    pub async fn rebuild_agent(&mut self) {
        #[cfg(feature = "advisor")]
        {
            crate::extras::advisor::update_client(self.client.clone());
            crate::extras::advisor::set_session_messages(self.session.messages.clone());
        }
        let new_agent = self
            .agent_build_ctx()
            .rebuild_agent(&self.session.model, *self.reasoning_enabled)
            .await;
        *self.agent = Some(new_agent);
    }

    /// Enter a different logical session and rebuild every agent-owned tool
    /// against that session's fresh, configuration-scoped runtime state.
    pub async fn replace_session(&mut self, mut session: Session) -> anyhow::Result<()> {
        let provider_changed = session.provider != self.session.provider;
        session.working_dir =
            compact_str::CompactString::new(self.workspace.root().to_string_lossy());
        session.initialize_read_tracker(self.cfg.deny_repeated_reads.unwrap_or(true));

        let next_client = if provider_changed {
            crate::provider::create_client(
                &session.provider,
                self.cli.api_key.as_deref(),
                &self.cfg.custom_providers_map(),
                self.cfg.api_keys.as_ref(),
            )?
        } else {
            self.client.clone()
        };
        let next_agent = self
            .build_agent_for_client(&next_client, &session.model, &session.read_tracker)
            .await;

        *self.client = next_client;
        *self.agent = Some(next_agent);
        *self.session = session;
        #[cfg(feature = "advisor")]
        {
            crate::extras::advisor::update_client(self.client.clone());
            crate::extras::advisor::set_session_messages(self.session.messages.clone());
        }
        Ok(())
    }

    pub async fn rebuild_agent_with_client(
        &mut self,
        provider: &str,
        new_reasoning: bool,
    ) -> Result<(), anyhow::Error> {
        *self.client = crate::provider::create_client(
            provider,
            self.cli.api_key.as_deref(),
            &self.cfg.custom_providers_map(),
            self.cfg.api_keys.as_ref(),
        )?;
        #[cfg(feature = "advisor")]
        {
            crate::extras::advisor::update_client(self.client.clone());
            crate::extras::advisor::set_session_messages(self.session.messages.clone());
        }
        let new_agent = self
            .agent_build_ctx()
            .rebuild_agent(&self.session.model, new_reasoning)
            .await;
        *self.agent = Some(new_agent);
        Ok(())
    }

    /// Switch to the quick-model configured in `[prompt_to_model]` for the
    /// given prompt name. Returns `true` if a model switch occurred (and the
    /// agent was rebuilt). When `false`, the caller should call
    /// `rebuild_agent()` to pick up other prompt changes (mode directive, etc.).
    pub async fn switch_to_prompt_model(&mut self, prompt_name: &str) -> bool {
        let qm_name = match self.cfg.resolve_prompt_model(prompt_name) {
            Some(name) => name,
            None => return false,
        };

        let qm = crate::config::quick_models_map(self.cfg);
        let Some(qmc) = qm.get(qm_name) else {
            return false;
        };

        let new_model = compact_str::CompactString::from(&*qmc.model);
        let provider_changed = qmc.provider != self.session.provider;

        // Update model before rebuild so the agent is built with it.
        self.session.model = new_model.clone();

        if provider_changed {
            match self
                .rebuild_agent_with_client(&qmc.provider, *self.reasoning_enabled)
                .await
            {
                Ok(()) => {
                    self.session.provider = compact_str::CompactString::from(&*qmc.provider);
                }
                Err(e) => {
                    let _ = self.renderer.write_line(
                        &format!(
                            "failed to switch provider for prompt '{}': {}",
                            prompt_name, e
                        ),
                        C_ERROR,
                    );
                    return false;
                }
            }
        } else {
            #[cfg(feature = "advisor")]
            {
                crate::extras::advisor::update_client(self.client.clone());
                crate::extras::advisor::set_session_messages(self.session.messages.clone());
            }
            let new_agent = self
                .agent_build_ctx()
                .rebuild_agent(&new_model, *self.reasoning_enabled)
                .await;
            *self.agent = Some(new_agent);
        }

        self.session.input_token_cost = qmc.input_token_cost;
        self.session.output_token_cost = qmc.output_token_cost;
        self.session
            .update_context_window(self.cfg.resolve_context_window(
                &self.session.provider,
                &self.session.model,
                &crate::config::quick_models_map(self.cfg),
            ));

        let _ = self.renderer.write_line(
            &format!(
                "switched to model: {} (from prompt '{}')",
                qm_name, prompt_name
            ),
            C_AGENT,
        );
        true
    }
}

/// Free-function variant of [`SlashCtx::switch_to_prompt_model`] for call
/// sites that don't have a `SlashCtx` (dot commands, chain transitions,
/// startup). Returns `true` if a model switch occurred.
pub(crate) async fn apply_prompt_model(
    prompt_name: &str,
    ui: &mut UiContext<'_>,
    agent: &mut Option<AnyAgent>,
    reasoning_enabled: bool,
    renderer: &mut Renderer,
) -> bool {
    let qm_name = match ui.cfg.resolve_prompt_model(prompt_name) {
        Some(name) => name,
        None => return false,
    };

    let qm = crate::config::quick_models_map(ui.cfg);
    let Some(qmc) = qm.get(qm_name) else {
        return false;
    };

    let new_model = compact_str::CompactString::from(&*qmc.model);
    let provider_changed = qmc.provider != ui.session.provider;

    ui.session.model = new_model.clone();

    if provider_changed {
        match crate::provider::create_client(
            &qmc.provider,
            ui.cli.api_key.as_deref(),
            &ui.cfg.custom_providers_map(),
            ui.cfg.api_keys.as_ref(),
        ) {
            Ok(new_client) => {
                ui.client = new_client;
                ui.session.provider = compact_str::CompactString::from(&*qmc.provider);
                // Fall through to rebuild agent below
            }
            Err(e) => {
                let _ = renderer.write_line(
                    &format!(
                        "failed to switch provider for prompt '{}': {}",
                        prompt_name, e
                    ),
                    C_ERROR,
                );
                return false;
            }
        }
    }

    #[cfg(feature = "advisor")]
    {
        crate::extras::advisor::update_client(ui.client.clone());
        crate::extras::advisor::set_session_messages(ui.session.messages.clone());
    }
    *agent = Some(
        ui.agent_build_ctx()
            .rebuild_agent(&new_model, reasoning_enabled)
            .await,
    );

    ui.session.input_token_cost = qmc.input_token_cost;
    ui.session.output_token_cost = qmc.output_token_cost;
    ui.session
        .update_context_window(ui.cfg.resolve_context_window(
            &ui.session.provider,
            &ui.session.model,
            &qm,
        ));

    let _ = renderer.write_line(
        &format!(
            "switched to model: {} (from prompt '{}')",
            qm_name, prompt_name
        ),
        C_AGENT,
    );
    true
}

pub(crate) fn write_ok(renderer: &mut Renderer, msg: impl std::fmt::Display) {
    let _ = renderer.write_line(&msg.to_string(), C_AGENT);
}

pub(crate) fn write_result(renderer: &mut Renderer, msg: impl std::fmt::Display) {
    let _ = renderer.write_line(&msg.to_string(), C_RESULT);
}

pub(crate) fn write_error(renderer: &mut Renderer, msg: impl std::fmt::Display) {
    let _ = renderer.write_line(&msg.to_string(), C_ERROR);
}

pub fn undo_last(session: &mut Session) -> usize {
    let len = session.messages.len();
    if len == 0 {
        return 0;
    }
    let removed = if session.messages[len - 1].role == MessageRole::Assistant {
        if len >= 2 && session.messages[len - 2].role == MessageRole::User {
            2
        } else {
            1
        }
    } else if session.messages[len - 1].role == MessageRole::User {
        1
    } else {
        0
    };
    // Rewind via the session helper so the context figure tracks the shortened
    // history (subtracts the removed turn from the calibration anchor rather than
    // going stale or resetting to a cold estimate) and the cut is reversible with
    // /redo.
    if removed > 0 {
        session.rewind_to(len - removed);
    }
    removed
}

fn summarizer_input_budget(context_window: u64, reserve: u64) -> u64 {
    if context_window == 0 {
        128_000
    } else {
        context_window.saturating_sub(reserve)
    }
}

pub async fn handle_compress(
    instructions: Option<&str>,
    auto: bool,
    run: &mut AgentRunState,
    renderer: &mut Renderer,
    ui: &mut UiContext<'_>,
    reasoning_enabled: bool,
) -> anyhow::Result<()> {
    // Mirror the auto-compaction trigger's reserve exactly (including memory's
    // effective_reserve) so the budget gate here can never disagree with the
    // gate that decided to call us.
    let qm = crate::config::quick_models_map(ui.cfg);
    #[cfg(feature = "memory")]
    let reserve = crate::extras::memory::effective_reserve(
        ui.cfg
            .resolve_reserve_tokens(&ui.session.model, &qm, ui.session.context_window),
        ui.context.memory.as_deref(),
    );
    #[cfg(not(feature = "memory"))]
    let reserve = ui
        .cfg
        .resolve_reserve_tokens(&ui.session.model, &qm, ui.session.context_window);
    let keep_recent = ui.cfg.resolve_keep_recent_tokens(ui.session.context_window);
    let max_tokens = ui.session.context_window.saturating_sub(reserve);
    let summarizer_input_budget = summarizer_input_budget(ui.session.context_window, reserve);

    let cut_idx = match plan_tui_compaction(ui.session, auto, max_tokens, keep_recent) {
        TuiCompactionGate::WithinBudget => return Ok(()),
        // Nothing old enough to summarize (everything is within keep_recent). This
        // is a real physical limit even when forced, so report it for manual runs;
        // stay silent under auto so an over-budget-but-unsummarizable turn does not
        // announce a no-op on every completion.
        TuiCompactionGate::NothingToSummarize => {
            if !auto {
                renderer.write_line("not enough conversation history to compact yet", C_AGENT)?;
            }
            return Ok(());
        }
        TuiCompactionGate::Cut(cut_idx) => cut_idx,
    };

    // Announce only once we know compression will actually run.
    if auto {
        renderer.write_line("auto-compacting...", crossterm::style::Color::DarkGrey)?;
    } else {
        renderer.write_line("compressing...", C_AGENT)?;
    }
    renderer.write_line("", crossterm::style::Color::White)?;

    let client = &ui.client;
    let (first_kept_index, tokens_before) = compact_session_with(
        ui.session,
        cut_idx,
        summarizer_input_budget,
        reserve,
        |model, messages, previous_summary, input_budget, response_budget| async move {
            client
                .compress_messages(
                    &model,
                    &messages,
                    previous_summary.as_deref(),
                    instructions,
                    input_budget,
                    response_budget,
                )
                .await
        },
        |summary, first_kept_index| {
            #[cfg(feature = "memory")]
            if let Some(pending) = run.pending_turn.as_mut() {
                pending.stage_memory_summary(summary.to_string(), Some(first_kept_index));
            } else {
                crate::extras::memory::flush_compaction_summary(
                    &crate::extras::memory::Mem::open(),
                    summary,
                    Some(first_kept_index),
                );
            }
            #[cfg(not(feature = "memory"))]
            let _ = (summary, first_kept_index);
        },
    )
    .await?;

    run.agent = Some(
        ui.agent_build_ctx()
            .rebuild_agent(&ui.session.model, reasoning_enabled)
            .await,
    );

    render_session(renderer, ui.session, ui.cli, ui.cfg, ui.context)?;
    renderer.write_line(
        &format!(
            "compressed {} messages (saved ~{} tokens)",
            first_kept_index, tokens_before,
        ),
        C_AGENT,
    )?;

    Ok(())
}

/// Outcome of the TUI compaction gate: whether the session is already within
/// budget (auto runs only), has nothing old enough to summarize, or should
/// summarize `messages[..cut_idx]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TuiCompactionGate {
    WithinBudget,
    NothingToSummarize,
    Cut(usize),
}

/// Auto-compaction only makes sense when actually over budget; manual
/// /compress is the user's explicit intent, so it skips the budget gate and
/// proceeds regardless of how full the context is.
pub(crate) fn plan_tui_compaction(
    session: &Session,
    auto: bool,
    max_tokens: u64,
    keep_recent: u64,
) -> TuiCompactionGate {
    if auto && session.effective_context_tokens() <= max_tokens {
        return TuiCompactionGate::WithinBudget;
    }
    match Session::select_compaction_cut(&session.messages, keep_recent) {
        0 => TuiCompactionGate::NothingToSummarize,
        cut_idx => TuiCompactionGate::Cut(cut_idx),
    }
}

/// Summarizes `session.messages[..cut_idx]` through `summarize` and replaces
/// the summarized prefix with the returned summary. The summarizer's count is
/// the length of the oldest prefix of the cut slice whose content it saw
/// (`cut_idx` with full coverage); exactly that prefix is drained, so
/// unsummarized messages are never deleted and summarized ones never linger
/// to re-trigger compaction. `stage_summary` observes the summary and drain
/// length before the session is mutated. Returns the drain length and the
/// estimated tokens it removed.
pub(crate) async fn compact_session_with<S, F>(
    session: &mut Session,
    cut_idx: usize,
    input_token_budget: u64,
    response_token_budget: u64,
    summarize: S,
    stage_summary: impl FnOnce(&str, usize),
) -> anyhow::Result<(usize, u64)>
where
    S: FnOnce(String, Vec<crate::session::SessionMessage>, Option<String>, u64, u64) -> F,
    F: std::future::Future<Output = anyhow::Result<(String, usize)>>,
{
    let model = session.model.to_string();
    let messages = session.messages[..cut_idx].to_vec();
    let previous_summary = session
        .compactions
        .last()
        .map(|compaction| compaction.summary.to_string());
    let (summary, messages_included) = summarize(
        model,
        messages,
        previous_summary,
        input_token_budget,
        response_token_budget,
    )
    .await?;
    let first_kept_index = Session::compaction_drain_len(cut_idx, messages_included)?;
    let tokens_before: u64 = session.messages[..first_kept_index]
        .iter()
        .map(|message| message.estimated_tokens)
        .sum();
    stage_summary(&summary, first_kept_index);
    session.compress(summary, first_kept_index, tokens_before);
    Ok((first_kept_index, tokens_before))
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_slash(
    text: &str,
    renderer: &mut Renderer,
    input: &mut InputEditor,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    slash: &mut SlashState,
    chain: &mut ChainState,
    terminal_guard: &mut TerminalGuard,
) -> anyhow::Result<()> {
    // `chain` only feeds `SlashCtx::loop_state`; without the loop feature it
    // has no consumer here.
    #[cfg(not(feature = "loop"))]
    let _ = &chain;
    let parts: SmallVec<[&str; 3]> = text.trim().splitn(3, ' ').collect();
    let mut ctx = SlashCtx {
        agent: &mut run.agent,
        client: &mut ui.client,
        renderer,
        session: ui.session,
        cli: ui.cli,
        cfg: ui.cfg,
        context: ui.context,
        workspace: &ui.workspace,
        show_reasoning: &mut slash.show_reasoning,
        reasoning_enabled: &mut slash.reasoning_enabled,
        is_running: &mut run.is_running,
        input,
        permission: &ui.permission,
        ask_tx: &ui.ask_tx,
        todo_tools_enabled: &mut slash.todo_tools_enabled,
        sandbox: &ui.sandbox,
        #[cfg(feature = "skills")]
        skill_services: &ui.skill_services,
        terminal_guard,
        #[cfg(feature = "loop")]
        loop_state: &mut chain.loop_state,
        #[cfg(feature = "mcp")]
        mcp_manager: ui.mcp_manager.as_ref(),
    };

    match parts[0] {
        "/provider" | "/model" | "/models" | "/models-add" | "/model-subagent"
        | "/models-subagent" => providers::handle(&parts, &mut ctx).await,
        "/prompt" | "/theme" | "/regen-prompts" | "/regen-themes" => {
            content::handle(&parts, &mut ctx).await
        }
        "/reasoning" | "/thinking" | "/mode" | "/toggle" | "/mcp" | "/editsys" | "/advisor" => {
            settings::handle(&parts, &mut ctx).await
        }
        "/sessions" | "/rename" | "/clear" | "/new" | "/undo" | "/redo" | "/rewind" | "/retry"
        | "/quit" | "/exit" | "/history" => session::handle(&parts, &mut ctx).await,
        #[cfg(feature = "export")]
        "/export" | "/import" | "/share" => session::handle(&parts, &mut ctx).await,
        "/help" => {
            help::handle(&parts, &mut ctx);
            Ok(())
        }
        "/welcome" | "/tutorial" => {
            help::handle_welcome(ctx.renderer);
            Ok(())
        }
        "/tutor" => help::handle_tutor(ctx.renderer, ctx.terminal_guard),
        "/add" | "/drop" | "/drop-all" => add::handle(&parts, &mut ctx).await,
        "/init" => init::handle(&parts, &mut ctx).await,
        "/review" => review::handle(&parts, &mut ctx).await,
        // `/memory write <target> <content>` and `/memory read daily <date>`
        // need more than the three fields the generic split above yields.
        "/memory" => memory::handle(&memory::split_command(text), &mut ctx).await,
        "/compress" | "/compact" | "/loop" | "/worktree" | "/wt-merge" | "/wt-exit" => {
            features::handle(&parts, &mut ctx).await
        }
        #[cfg(feature = "hooks")]
        "/hooks" => hooks::handle(&parts, &mut ctx).await,
        _ => {
            write_error(
                ctx.renderer,
                format!("unknown command: {} (try /help)", parts[0]),
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod compaction_budget_tests {
    use super::{
        TuiCompactionGate, compact_session_with, plan_tui_compaction, summarizer_input_budget,
    };
    use crate::session::{MessageRole, Session};

    #[test]
    fn known_exhausted_window_does_not_use_unknown_window_fallback() {
        assert_eq!(summarizer_input_budget(8_000, 8_000), 0);
        assert_eq!(summarizer_input_budget(0, 0), 128_000);
    }

    fn over_budget_session() -> Session {
        let mut session = Session::new("openai", "model", 100, "");
        session.overhead_tokens = 50;
        session.add_message(MessageRole::User, &"a".repeat(80));
        session.add_message(MessageRole::Assistant, &"b".repeat(80));
        session.add_message(MessageRole::User, &"c".repeat(16));
        session
    }

    const MAX_TOKENS: u64 = 80;
    // Below the ~4-token estimate of the 16-char tail so exactly two messages are cut.
    const KEEP_RECENT: u64 = 3;

    #[tokio::test]
    async fn tui_compaction_drains_whole_cut_and_second_pass_is_a_noop() {
        let mut session = over_budget_session();
        let tokens_before_expected =
            session.messages[0].estimated_tokens + session.messages[1].estimated_tokens;
        assert_eq!(
            plan_tui_compaction(&session, true, MAX_TOKENS, KEEP_RECENT),
            TuiCompactionGate::Cut(2)
        );

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let staged = std::cell::RefCell::new(None);
        let (first_kept_index, tokens_before) = compact_session_with(
            &mut session,
            2,
            80,
            20,
            |model, messages, previous_summary, input_budget, response_budget| async move {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(model, "model");
                assert_eq!(messages.len(), 2);
                assert!(previous_summary.is_none());
                assert_eq!(input_budget, 80);
                assert_eq!(response_budget, 20);
                Ok(("TUI_SUMMARY".to_string(), 2usize))
            },
            |summary, first_kept_index| {
                *staged.borrow_mut() = Some((summary.to_string(), first_kept_index));
            },
        )
        .await
        .unwrap();

        assert_eq!(first_kept_index, 2);
        assert_eq!(tokens_before, tokens_before_expected);
        assert_eq!(
            staged.into_inner(),
            Some(("TUI_SUMMARY".to_string(), 2)),
            "memory staging must receive the drained prefix length"
        );
        // Three messages minus the two-message cut plus the summary.
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(session.messages[0].content, "TUI_SUMMARY");
        assert_eq!(session.messages[1].content, "c".repeat(16));
        assert_eq!(session.compactions.len(), 1);
        assert_eq!(session.compactions[0].summarized_count, 2);

        // Between-turn auto-compaction must not fire again now that the
        // session fits: the gate short-circuits before any summarizer call.
        assert_eq!(
            plan_tui_compaction(&session, true, MAX_TOKENS, KEEP_RECENT),
            TuiCompactionGate::WithinBudget
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn tui_compaction_with_partial_coverage_keeps_unsummarized_messages() {
        let mut session = over_budget_session();
        let (first_kept_index, tokens_before) = compact_session_with(
            &mut session,
            2,
            80,
            20,
            |_, messages, _, _, _| async move {
                assert_eq!(messages.len(), 2);
                Ok(("PARTIAL".to_string(), 1usize))
            },
            |_, _| {},
        )
        .await
        .unwrap();

        assert_eq!(first_kept_index, 1);
        assert_eq!(
            tokens_before,
            Session::estimate_tokens(&"a".repeat(80)),
            "tokens_before must cover only the drained prefix"
        );
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].content, "PARTIAL");
        assert_eq!(session.messages[1].content, "b".repeat(80));
        assert_eq!(session.messages[2].content, "c".repeat(16));
    }

    #[tokio::test]
    async fn tui_compaction_rejects_a_summary_that_covers_nothing() {
        let mut session = over_budget_session();
        let error = compact_session_with(
            &mut session,
            2,
            80,
            20,
            |_, _, _, _, _| async move { Ok(("EMPTY".to_string(), 0usize)) },
            |_, _| panic!("nothing must be staged when nothing is drained"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("nothing to drain"));
        assert_eq!(session.messages.len(), 3);
        assert!(session.compactions.is_empty());
    }

    #[test]
    fn manual_compress_skips_the_budget_gate_but_respects_keep_recent() {
        let mut session = Session::new("openai", "model", 100_000, "");
        session.add_message(MessageRole::User, "old");
        session.add_message(MessageRole::Assistant, "recent");
        assert_eq!(
            plan_tui_compaction(&session, true, 99_000, 1),
            TuiCompactionGate::WithinBudget
        );
        assert_eq!(
            plan_tui_compaction(&session, false, 99_000, 1),
            TuiCompactionGate::Cut(1)
        );
        assert_eq!(
            plan_tui_compaction(&session, false, 99_000, 1_000),
            TuiCompactionGate::NothingToSummarize
        );
    }
}
