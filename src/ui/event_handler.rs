use compact_str::CompactString;
use crossterm::style::Color;
use rig::completion::Message;

use crate::cli::Cli;
use crate::config::ResolvedShowToolDetails;
use crate::event::AgentEvent;
#[cfg(any(feature = "loop", feature = "goal"))]
use crate::event::UserEvent;
use crate::provider::AnyAgent;
use crate::session::{MessageRole, Session};
use crate::ui::events::sanitize_output;
use crate::ui::feed::BlockStyle;
use crate::ui::renderer::Renderer;
use crate::ui::slash::handle_compress;
use crate::ui::state::{AgentRunState, ChainState, SlashState, UiContext};

#[cfg(any(feature = "loop", feature = "goal"))]
use super::C_AGENT;
use super::{C_ERROR, C_TOOL};

/// Build the main agent on first use, lazily connecting MCP as well. Callers
/// only reach the build when `agent` is `None`, so MCP connects at most once.
pub async fn ensure_agent(
    agent: &mut Option<AnyAgent>,
    ui: &mut UiContext<'_>,
    reasoning_enabled: bool,
) {
    if agent.is_some() {
        return;
    }
    #[cfg(feature = "mcp")]
    if ui.cli.mcp_is_eligible(ui.cfg) {
        crate::ui::ensure_mcp_manager(&mut ui.mcp_manager, ui.cfg, &ui.workspace).await;
    }
    *agent = Some(
        ui.agent_build_ctx()
            .rebuild_agent(&ui.session.model, reasoning_enabled)
            .await,
    );
    // Keep the pre-calibration context estimate in sync with the preamble we
    // just built (system prompt + tools + context files).
    #[cfg(feature = "goal")]
    let goal_block =
        crate::extras::goal::prompt::goal_block(ui.session.goal_store.snapshot().as_ref());
    #[cfg(not(feature = "goal"))]
    let goal_block: Option<String> = None;
    ui.session.overhead_tokens = crate::agent::builder::estimate_overhead(
        ui.context,
        reasoning_enabled,
        ui.cli,
        ui.cfg,
        &ui.sandbox,
        goal_block.as_deref(),
    );
}

pub async fn handle_agent_event(
    event: AgentEvent,
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    slash: &SlashState,
    chain: &mut ChainState,
    #[cfg(any(feature = "loop", feature = "goal"))] validation_tx: &tokio::sync::mpsc::Sender<
        UserEvent,
    >,
) -> anyhow::Result<()> {
    // Every event contributes to the round the gate will judge, so the goal
    // sees the run exactly as it happened rather than as the final response
    // describes it.
    #[cfg(feature = "goal")]
    if ui.session.goal_store.is_live() {
        run.goal_round
            .get_or_insert_with(crate::extras::goal::driver::RoundCollector::new)
            .observe(&event);
    }
    match event {
        AgentEvent::Reasoning(text) => {
            if !slash.show_reasoning {
                return Ok(());
            }
            if !run.agent_line_started {
                renderer.write("< ", Color::DarkMagenta)?;
                run.agent_line_started = true;
            }
            let safe = sanitize_output(&text);
            renderer.write(&safe, Color::DarkMagenta)?;
            run.was_reasoning = true;
        }
        AgentEvent::Token(text) => {
            if run.was_reasoning {
                renderer.write_line("", Color::White)?;
                run.agent_line_started = false;
                run.was_reasoning = false;
                run.response_buf.clear();
                run.response_start_block = None;
            }
            let safe = sanitize_output(&text);
            run.response_buf.push_str(&safe);

            if run.response_buf.is_empty() {
                return Ok(());
            }

            // Append the token to the tracked running block, never to whatever
            // block happens to be last: `/btw` answers and "queued:" notices
            // push blocks mid-stream. Layout renders the unfinished tail line
            // as plain text and parses markdown only for completed lines.
            let idx = match run.response_start_block {
                Some(idx) if renderer.feed().is_streaming(idx) => idx,
                _ => {
                    renderer.feed_mut().push_streaming_block(BlockStyle::Agent);
                    let idx = renderer.feed().block_count() - 1;
                    run.response_start_block = Some(idx);
                    idx
                }
            };
            renderer.feed_mut().append_to(idx, &safe);

            // Throttle repaints: redraw when a line completed (markdown
            // structure changes at line boundaries) or while the buffer is
            // small. The final full parse happens in handle_agent_done.
            if run.response_buf.len() >= 200 && !run.response_buf.ends_with('\n') {
                return Ok(());
            }

            renderer.render_viewport()?;
            run.agent_line_started = true;
        }
        AgentEvent::ToolCall { id, name, args } => {
            run.was_reasoning = false;
            finalize_response_segment(renderer, run)?;
            if run.agent_line_started {
                renderer.write_line("", Color::White)?;
                run.agent_line_started = false;
            }
            if !run.response_buf.is_empty() {
                ui.session
                    .add_message(MessageRole::Assistant, &run.response_buf);
            }
            run.response_buf.clear();
            run.response_start_block = None;
            ui.session.add_tool_call_with_id(&id, &name, &args);
            save_session_if_settled(ui.session, ui.cli, run, renderer)?;
            let line = format!(
                "◈ {}",
                crate::ui::utils::format_tool_call_summary(&name, &args)
            );
            renderer.write_line(&sanitize_output(&line), C_TOOL)?;
        }
        #[cfg(feature = "subagents")]
        AgentEvent::SubagentToolCall { name, args } => {
            // The raw `args` go to the session, never the rendered summary
            // below: they are persisted as the structured payload
            // `convert_history` replays as a real tool call, and flattening
            // them here would put the nested call back in assistant prose.
            ui.session.add_subagent_tool_call(&name, &args);
            save_session_if_settled(ui.session, ui.cli, run, renderer)?;
            let line = format!(
                "⌥ {}",
                crate::ui::utils::format_tool_call_summary(&name, &args)
            );
            renderer.write_line(&sanitize_output(&line), C_TOOL)?;
        }
        #[cfg(feature = "subagents")]
        AgentEvent::SubagentStarted { agent_type, source } => {
            renderer.write_line(
                &sanitize_output(&format!("⌥ specialist {agent_type} ({source})")),
                C_TOOL,
            )?;
        }
        AgentEvent::ToolResult { id, name, output } => {
            let (_, artifact) = ui
                .session
                .add_tool_result_with_id_and_artifact(&id, &name, &output);
            if let (Some(pending), Some(path)) = (run.pending_turn.as_mut(), artifact) {
                pending.record_tool_output(path);
            }
            save_session_if_settled(ui.session, ui.cli, run, renderer)?;
            if crate::agent::runner::is_tool_loop_notice(&output) {
                // The following ToolLoop event renders the diagnostic with a
                // dedicated style; keep this event for canonical persistence.
            } else if name == "todo_write" {
                renderer.write_line(&sanitize_output(&output), C_TOOL)?;
            } else {
                let show_details = ui
                    .cfg
                    .show_tool_details
                    .as_ref()
                    .map(|s| s.resolve())
                    .unwrap_or(ResolvedShowToolDetails::Limited(3));
                match show_details {
                    ResolvedShowToolDetails::Off => {}
                    ResolvedShowToolDetails::Limited(max_lines) => {
                        let sanitized = sanitize_output(&output);
                        let char_count = sanitized.chars().count();
                        let lines: Vec<&str> = sanitized.lines().collect();
                        if lines.len() > max_lines {
                            let shown = lines[..max_lines].join("\n");
                            let summary = format!(
                                "◈ result ({} chars, {} lines, showing {}):\n{}",
                                char_count,
                                lines.len(),
                                max_lines,
                                shown
                            );
                            renderer.write_line(&summary, Color::DarkGrey)?;
                        } else {
                            let summary =
                                format!("◈ result ({} chars):\n{}", char_count, sanitized);
                            renderer.write_line(&summary, Color::DarkGrey)?;
                        }
                    }
                    ResolvedShowToolDetails::Unlimited => {
                        let sanitized = sanitize_output(&output);
                        let char_count = sanitized.chars().count();
                        let summary = format!("◈ result ({} chars):\n{}", char_count, sanitized);
                        renderer.write_line(&summary, Color::DarkGrey)?;
                    }
                }
            }
        }
        AgentEvent::ToolLoop { name, message } => {
            run.was_reasoning = false;
            finalize_response_segment(renderer, run)?;
            if run.agent_line_started {
                renderer.write_line("", Color::White)?;
                run.agent_line_started = false;
            }
            renderer.write_line(
                &sanitize_output(&format!("◈ loop guard ({name}): {message}")),
                Color::Yellow,
            )?;
        }
        AgentEvent::Verification {
            attempt,
            max,
            passed,
            output,
        } => {
            run.was_reasoning = false;
            finalize_response_segment(renderer, run)?;
            if run.agent_line_started {
                renderer.write_line("", Color::White)?;
                run.agent_line_started = false;
            }
            if !passed {
                // A rejected completion starts a distinct response segment on
                // the continuation turn. Keep the cumulative buffer for final
                // session reconciliation, but never append new tokens to the
                // already-finalized markdown block above.
                run.response_start_block = None;
            }
            let status = if passed { "passed" } else { "failed" };
            renderer.write_line(
                &format!("◈ verification {status} ({attempt}/{max})"),
                if passed { Color::Green } else { C_ERROR },
            )?;
            if !passed {
                renderer.write_line(&sanitize_output(&output), Color::DarkGrey)?;
            }
        }
        AgentEvent::Done {
            response,
            interactions,
        } => {
            run.pending_compaction_pressure = None;
            handle_agent_done(
                response,
                interactions,
                renderer,
                run,
                ui,
                chain,
                #[cfg(any(feature = "loop", feature = "goal"))]
                validation_tx,
            )
            .await?;
        }
        AgentEvent::UsageDelta {
            usage,
            context_complete,
            ..
        } => {
            let anthropic_native = ui.cfg.is_anthropic_native(&ui.session.provider);
            apply_usage_delta(
                ui.session,
                usage,
                anthropic_native,
                context_complete,
                run.request_tool_results_cleared,
            );
        }
        AgentEvent::Retrying { attempt, max } => {
            run.was_reasoning = false;
            finalize_response_segment(renderer, run)?;
            if run.agent_line_started {
                renderer.write_line("", Color::White)?;
                run.agent_line_started = false;
            }
            run.response_start_block = None;
            renderer.write_line(&format!("retrying... ({}/{})", attempt, max), Color::Yellow)?;
        }
        AgentEvent::CompactionBoundary { .. } => {}
        AgentEvent::Error {
            message: e,
            interactions,
        } => {
            // Live tool events carry lifecycle IDs only. Even a failed turn
            // needs the canonical provider IDs and reasoning for continuation.
            // Adopt before fallible presentation or settlement so App unwind
            // preserves the same replay metadata as a successful turn.
            ui.session.adopt_provider_tool_identity(&interactions);
            run.was_reasoning = false;
            run.is_running = false;
            run.pending_compaction_pressure = None;
            if let Some(ss) = ui.status_signals.as_ref() {
                ss.send_stop();
            }
            run.compaction_decision_tx = None;
            run.agent_line_started = false;
            finalize_response_segment(renderer, run)?;
            crate::ui::preserve_pending_main_turn_progress(run, ui.session);
            run.response_buf.clear();
            run.response_start_block = None;
            run.settle(std::time::Duration::from_secs(5)).await?;
            save_session_if_settled(ui.session, ui.cli, run, renderer)?;
            let safe = sanitize_output(&e);
            renderer.write_line(&format!("error: {}", safe), C_ERROR)?;
        }
    }
    Ok(())
}

fn apply_usage_delta(
    session: &mut Session,
    usage: crate::event::UsageDelta,
    anthropic_native: bool,
    context_complete: bool,
    cleared_tool_results: usize,
) {
    // Real provider-reported usage is the status/context source of truth. Use
    // the cache-inclusive prompt size for native Anthropic cache hits.
    let context_input_tokens = Session::real_input_tokens(
        anthropic_native,
        usage.input_tokens,
        usage.total_tokens,
        usage.output_tokens,
        usage.cached_input_tokens,
        usage.cache_creation_input_tokens,
    );
    session.charge_usage_delta(usage, anthropic_native);
    if context_complete {
        let real = context_input_tokens.saturating_add(usage.output_tokens);
        if real > session.total_estimated_tokens {
            session.total_estimated_tokens = real;
        }
        session.set_calibration_with_cleared_tool_results(
            context_input_tokens,
            usage.output_tokens,
            cleared_tool_results,
        );
    }
}

fn save_session_if_settled(
    session: &Session,
    cli: &Cli,
    run: &AgentRunState,
    renderer: &mut Renderer,
) -> anyhow::Result<()> {
    if let Err(e) = crate::ui::persist_session_if_settled(session, !cli.no_session, run) {
        renderer.write_line(&format!("warning: failed to save session: {}", e), C_ERROR)?;
    }
    Ok(())
}

/// Commit a completed turn's final assistant response to the session.
///
/// `interactions` is the runner's canonical provider transcript for the turn
/// (`AgentEvent::Done { interactions }`). The interactive path has already
/// persisted every tool call and result live, as it streamed
/// (`handle_agent_event`'s `ToolCall` / `ToolResult` arms record the real tool
/// name under the runner's lifecycle id, and intermediate assistant text
/// before each call), so the batch is deliberately NOT written here: doing so
/// persisted every tool interaction twice, the second copy attributed to
/// "unknown" (mini-agent-h41j). Only the headless `-p` path, which has no live
/// events, persists from the batch (`print::persist_headless_turn`).
///
/// The batch is still the only place the provider's own tool identity and its
/// reasoning items are visible — `AgentEvent::ToolCall` carries rig's internal
/// lifecycle id and no reasoning at all — so the live records adopt both from
/// it here (mini-agent-wzv1). That is what makes an interactive session persist
/// the same `call_id` headless `-p` does, and what lets a resumed turn replay
/// a native `function_call` item with the `reasoning` item the Responses API
/// demands alongside it.
pub(crate) fn commit_turn_response(
    session: &mut Session,
    response: &str,
    interactions: &[Message],
) {
    let adopted = session.adopt_provider_tool_identity(interactions);
    tracing::debug!(
        canonical_interactions = interactions.len(),
        adopted_provider_identities = adopted,
        "committing turn response; tool records were persisted live"
    );
    session.add_message(MessageRole::Assistant, response);
}

async fn handle_agent_done(
    response: CompactString,
    interactions: Vec<Message>,
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    _chain: &mut ChainState,
    #[cfg(any(feature = "loop", feature = "goal"))] validation_tx: &tokio::sync::mpsc::Sender<
        UserEvent,
    >,
) -> anyhow::Result<()> {
    run.was_reasoning = false;

    // Commit the provider's completed response and accounting before any
    // fallible presentation or post-processing. The App wrapper can then
    // persist this valid success even if rendering, compaction, validation
    // startup, or worktree-return presentation fails afterward.
    commit_turn_response(ui.session, &response, &interactions);
    ui.session.reanchor_calibration_to_current_messages();
    run.settle(std::time::Duration::from_secs(5)).await?;

    finalize_response_segment(renderer, run)?;
    if run.response_buf.is_empty() && !run.agent_line_started {
        renderer.feed_mut().push_line(BlockStyle::Agent, "< ");
    }

    renderer.write_line("", Color::White)?;
    renderer.write_line("", Color::White)?;
    run.agent_line_started = false;
    run.response_buf.clear();
    run.response_start_block = None;

    #[cfg(feature = "loop")]
    let loop_running = ui.session.goal_store.is_active();
    #[cfg(not(feature = "loop"))]
    let loop_running = false;

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

    if should_auto_compact_between_turns(
        ui.cfg.resolve_compact_enabled(),
        ui.session.needs_compaction_after_tool_result_pruning(
            reserve,
            ui.cfg.resolve_keep_recent_tool_results(),
        ),
        loop_running,
    ) {
        let compress_result = handle_compress(None, true, run, renderer, ui, true).await;
        if let Err(e) = compress_result {
            renderer.write_line(&format!("auto-compact error: {}", e), C_ERROR)?;
        }
    }

    // `Done` can still be an intermediate state for a loop validation or a
    // following loop iteration. Keep the pre-turn disk snapshot authoritative
    // until App::finalize_turn observes that the whole user turn has settled.
    if let Err(e) = crate::ui::persist_session_if_settled(ui.session, !ui.cli.no_session, run) {
        renderer.write_line(&format!("warning: failed to save session: {}", e), C_ERROR)?;
    }
    run.is_running = false;
    if let Some(ss) = ui.status_signals.as_ref() {
        ss.send_stop();
    }

    // A goal round ends where an ordinary turn would: judge it, then either
    // launch the next round or hand control back with the reason visible.
    #[cfg(feature = "goal")]
    if Box::pin(run_goal_round(renderer, run, ui, validation_tx)).await? {
        return Ok(());
    }

    Ok(())
}

fn should_auto_compact_between_turns(
    compact_enabled: bool,
    needs_compaction: bool,
    _loop_running: bool,
) -> bool {
    // A completed assistant response is a safe boundary regardless of whether
    // the next action is ordinary input or another loop iteration.
    compact_enabled && needs_compaction
}

/// Settle the goal round that just ended.
///
/// Splits at the gate's two phases. Anything the round alone decides is decided
/// here; a completion claim that needs commands or a judge has its verification
/// started on its own task, so the interface stays responsive — including to an
/// interrupt — while a test suite runs.
///
/// Returns `true` when the turn is not over: either a new round was launched or
/// verification is still running.
#[cfg(feature = "goal")]
async fn run_goal_round(
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    verification_tx: &tokio::sync::mpsc::Sender<UserEvent>,
) -> anyhow::Result<bool> {
    use crate::extras::goal::gate;

    let Some(collector) = run.goal_round.take() else {
        return Ok(false);
    };
    let goal = ui.session.goal_store.snapshot();
    let Some(goal) = goal.filter(|g| !g.status.is_terminal()) else {
        return Ok(false);
    };

    let mut summary = collector.finish(&goal, gate::RoundEnd::Done, 0);
    summary.open_todos = ui
        .session
        .todos
        .snapshot()
        .iter()
        .filter(|item| !matches!(item.status.as_str(), "completed" | "cancelled"))
        .count();
    summary.verify_configured = ui
        .cfg
        .verify_command
        .as_deref()
        .is_some_and(|command| !command.trim().is_empty());

    match gate::gate_pre(&goal, &summary) {
        gate::Step::Decided(decision) => {
            Box::pin(finish_goal_round(
                renderer,
                run,
                ui,
                summary,
                decision,
                crate::extras::goal::driver::Verification::default(),
            ))
            .await
        }
        gate::Step::Verify(request) => {
            Box::pin(start_goal_verification(
                renderer,
                run,
                ui,
                verification_tx,
                goal,
                summary,
                request,
            ))
            .await
        }
    }
}

/// Run a round's verification on its own task.
#[cfg(feature = "goal")]
#[allow(clippy::too_many_arguments)]
async fn start_goal_verification(
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    verification_tx: &tokio::sync::mpsc::Sender<UserEvent>,
    goal: crate::extras::goal::Goal,
    summary: crate::extras::goal::gate::RoundSummary,
    request: crate::extras::goal::gate::VerifyRequest,
) -> anyhow::Result<bool> {
    let sandbox = ui.sandbox.clone();
    let cfg = ui.cfg.clone();
    let judge = crate::extras::goal::judge::resolve(
        &goal.judge,
        ui.cfg,
        &ui.session.provider,
        &ui.session.model,
    );
    let transcript = crate::extras::goal::judge::transcript_tail(ui.session);
    let client = ui.client.clone();

    run.goal_gate_generation = run
        .goal_gate_generation
        .checked_add(1)
        .expect("goal verification generation exhausted");
    let operation_id = crate::event::ValidationOperationId(run.goal_gate_generation);
    renderer.write_line("◈ verifying the goal", C_AGENT)?;

    let tx = verification_tx.clone();
    let pending_request = request.clone();
    let task = tokio::spawn(async move {
        let checks = crate::extras::goal::checks::run(&goal, &request, &sandbox, &cfg).await;
        // A failing check already decides the claim, and the gate returns
        // before it looks at the judge: asking anyway spends a model call on
        // an answer nobody reads.
        let checks_rejected = checks.as_ref().is_some_and(|o| !o.all_passed);
        let judged = match judge {
            Some(resolved) if request.run_judge && !checks_rejected => Some(
                crate::extras::goal::judge::ask_with_transcript(
                    &goal,
                    &request,
                    &resolved,
                    &client,
                    &transcript,
                    &cfg,
                )
                .await,
            ),
            _ => None,
        };
        let _ = tx
            .send(UserEvent::GoalVerificationDone(Box::new(
                crate::event::GoalVerificationEvent {
                    operation_id,
                    checks,
                    judge: judged,
                },
            )))
            .await;
    });
    // Interrupting aborts the task, which drops the running command and reaps
    // its process group; the command's own timeout bounds it otherwise.
    run.main_abort = Some(task.abort_handle());
    run.is_running = true;
    run.pending_goal_gate = Some(crate::ui::state::PendingGoalGate {
        operation_id,
        abort: task.abort_handle(),
        summary,
        request: pending_request,
    });
    Ok(true)
}

/// Resume a round whose verification has come back.
#[cfg(feature = "goal")]
pub(crate) async fn handle_goal_verification_event(
    event: Box<crate::event::GoalVerificationEvent>,
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
) -> anyhow::Result<bool> {
    // A result from a superseded round is ignored: its generation no longer
    // matches the one in flight.
    let Some(pending) = run
        .pending_goal_gate
        .take_if(|pending| pending.operation_id == event.operation_id)
    else {
        return Ok(false);
    };
    let _ = &pending.abort;
    run.is_running = false;
    let decision = {
        let Some(goal) = ui.session.goal_store.snapshot() else {
            return Ok(false);
        };
        crate::extras::goal::gate::gate_post(
            &goal,
            &pending.summary,
            &pending.request,
            event.checks.as_ref(),
            event.judge.as_ref(),
        )
    };
    Box::pin(finish_goal_round(
        renderer,
        run,
        ui,
        pending.summary,
        decision,
        crate::extras::goal::driver::Verification {
            request: Some(pending.request),
            checks: event.checks,
            judge: event.judge,
        },
    ))
    .await
}

/// Fold a decided round into the goal and either relaunch or hand back control.
#[cfg(feature = "goal")]
async fn finish_goal_round(
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    summary: crate::extras::goal::gate::RoundSummary,
    decision: crate::extras::goal::gate::GateDecision,
    verification: crate::extras::goal::driver::Verification,
) -> anyhow::Result<bool> {
    use crate::extras::goal::driver::{self, RoundOutcome};

    #[cfg(feature = "skills")]
    crate::extras::goal::driver::set_outcome_recorder(
        crate::provider::goal_outcome_recorder(ui.cli, ui.cfg, &ui.skill_services, &ui.workspace)
            .await,
    );

    let outcome = driver::apply_decision(&ui.session.goal_store, &summary, decision, &verification);

    match outcome {
        RoundOutcome::Inactive => Ok(false),
        RoundOutcome::Stopped { line, .. } => {
            renderer.write_line(&line, C_AGENT)?;
            if let Err(e) =
                crate::ui::persist_session_if_settled(ui.session, !ui.cli.no_session, run)
            {
                renderer.write_line(&format!("warning: failed to save session: {e}"), C_ERROR)?;
            }
            Ok(false)
        }
        RoundOutcome::Relaunch { relaunch, line } => {
            renderer.write_line(&line, C_AGENT)?;
            if let Err(e) =
                crate::ui::persist_session_if_settled(ui.session, !ui.cli.no_session, run)
            {
                renderer.write_line(&format!("warning: failed to save session: {e}"), C_ERROR)?;
            }
            let keep_recent = ui.cfg.resolve_keep_recent_tool_results();
            run.request_tool_results_cleared =
                ui.session.tool_results_cleared_for_retention(keep_recent);
            let history: std::sync::Arc<[rig::completion::Message]> = match relaunch.history {
                driver::HistoryMode::Retained => {
                    crate::agent::runner::convert_history_shared_with_tool_result_retention(
                        ui.session,
                        keep_recent,
                    )
                }
                // A restarted round is deliberately amnesiac: its prompt
                // carries the objective and a harness-built summary instead.
                driver::HistoryMode::Empty => std::sync::Arc::from(Vec::new()),
            };
            run.agent = Some(
                ui.agent_build_ctx()
                    .rebuild_agent(&ui.session.model, true)
                    .await,
            );
            let runner = run
                .agent
                .as_ref()
                .expect("goal agent was rebuilt")
                .clone()
                .spawn_runner(
                    relaunch.prompt,
                    history,
                    ui.cfg.retry.clone(),
                    #[cfg(feature = "hooks")]
                    None,
                )
                .await;
            run.compaction_decision_tx = runner.compaction_decision_tx;
            run.agent_rx = Some(runner.event_rx);
            run.main_abort = Some(runner.abort_handle);
            run.is_running = true;
            run.goal_round = Some(driver::RoundCollector::new());
            if let Some(signals) = ui.status_signals.as_ref() {
                signals.send_start();
            }
            Ok(true)
        }
    }
}

fn finalize_response_segment(
    renderer: &mut Renderer,
    run: &mut AgentRunState,
) -> anyhow::Result<()> {
    if run.response_buf.is_empty() {
        return Ok(());
    }

    if let Some(start) = run.response_start_block {
        // Finalize the full segment (including its last line) as markdown in
        // place. Blocks interleaved after it (`/btw` answers, queue notices)
        // are left alone: tokens were appended by index, so nothing is glued
        // to them and nothing is lost by keeping them.
        renderer.feed_mut().finalize_block(start);
    } else {
        renderer
            .feed_mut()
            .push_block(BlockStyle::Agent, run.response_buf.as_str());
    }
    renderer.render_viewport()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{apply_usage_delta, should_auto_compact_between_turns};
    use crate::event::UsageDelta;
    use crate::session::Session;

    #[test]
    fn loop_iterations_allow_between_turn_compaction() {
        // Loop continuation is intentionally absent from this gate: a
        // completed response is a safe between-iteration compaction boundary.
        assert!(should_auto_compact_between_turns(true, true, true));
        assert!(!should_auto_compact_between_turns(false, true, true));
    }

    #[test]
    fn ui_usage_delta_accounting_updates_status_cost_and_persisted_totals_once() {
        let mut session = Session::new("anthropic", "claude-sonnet", 200_000, "");
        session.input_token_cost = 2.0;
        session.output_token_cost = 10.0;
        let first = UsageDelta {
            input_tokens: 10,
            output_tokens: 2,
            cached_input_tokens: 7,
            cache_creation_input_tokens: 3,
            ..UsageDelta::default()
        };
        let second = UsageDelta {
            input_tokens: 20,
            output_tokens: 4,
            cached_input_tokens: 5,
            cache_creation_input_tokens: 1,
            ..UsageDelta::default()
        };

        apply_usage_delta(&mut session, first, true, true, 0);
        apply_usage_delta(&mut session, second, true, true, 0);

        assert_eq!(session.total_input_tokens, 30);
        assert_eq!(session.total_output_tokens, 6);
        assert_eq!(session.total_cached_input_tokens, 12);
        assert_eq!(session.total_cache_creation_input_tokens, 4);
        let expected_cost = [first, second]
            .into_iter()
            .map(|usage| {
                crate::pricing::estimate_cost(
                    crate::pricing::billable_input_tokens(
                        true,
                        usage.input_tokens,
                        usage.cached_input_tokens,
                        usage.cache_creation_input_tokens,
                    ),
                    usage.output_tokens,
                    session.input_token_cost,
                    session.output_token_cost,
                )
            })
            .sum::<f64>();
        assert!((session.total_cost - expected_cost).abs() < f64::EPSILON);
        assert_eq!(session.effective_context_tokens(), 30);

        apply_usage_delta(
            &mut session,
            UsageDelta {
                output_tokens: 2,
                ..UsageDelta::default()
            },
            true,
            false,
            0,
        );
        assert_eq!(session.total_output_tokens, 8);
        assert_eq!(
            session.effective_context_tokens(),
            30,
            "a field-wise terminal reconciliation must not replace the last complete context snapshot"
        );
    }

    #[test]
    fn ui_usage_delta_saturates_context_and_persisted_totals() {
        let mut session = Session::new("anthropic", "claude-sonnet", u64::MAX, "");
        session.total_input_tokens = u64::MAX - 1;
        session.total_output_tokens = u64::MAX - 1;
        session.total_cached_input_tokens = u64::MAX - 1;
        session.total_cache_creation_input_tokens = u64::MAX - 1;

        apply_usage_delta(
            &mut session,
            UsageDelta {
                input_tokens: u64::MAX,
                output_tokens: 10,
                cached_input_tokens: 10,
                cache_creation_input_tokens: 10,
                ..UsageDelta::default()
            },
            true,
            true,
            0,
        );

        assert_eq!(session.total_input_tokens, u64::MAX);
        assert_eq!(session.total_output_tokens, u64::MAX);
        assert_eq!(session.total_cached_input_tokens, u64::MAX);
        assert_eq!(session.total_cache_creation_input_tokens, u64::MAX);
        assert_eq!(session.total_estimated_tokens, u64::MAX);
        assert_eq!(session.effective_context_tokens(), u64::MAX);
    }
}
