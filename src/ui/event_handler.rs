use compact_str::CompactString;
use crossterm::style::Color;

use crate::agent::tools::todo::TODO_LIST;
use crate::cli::Cli;
use crate::config::ResolvedShowToolDetails;
use crate::event::AgentEvent;
#[cfg(feature = "loop")]
use crate::event::{LoopValidationEvent, UserEvent};
use crate::provider::AnyAgent;
use crate::session::{MessageRole, Session};
use crate::ui::events::sanitize_output;
use crate::ui::feed::BlockStyle;
use crate::ui::renderer::Renderer;
use crate::ui::slash::handle_compress;
use crate::ui::state::{AgentRunState, ChainState, SlashState, TurnUsage, UiContext};

#[cfg(any(feature = "git-worktree", feature = "loop"))]
use super::C_AGENT;
#[cfg(feature = "git-worktree")]
use super::apply_current_prompt_mode;
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
    crate::ui::ensure_mcp_manager(&mut ui.mcp_manager, ui.cfg).await;
    *agent = Some(
        ui.agent_build_ctx()
            .rebuild_agent(&ui.session.model, reasoning_enabled)
            .await,
    );
    // Keep the pre-calibration context estimate in sync with the preamble we
    // just built (system prompt + tools + context files).
    ui.session.overhead_tokens =
        crate::agent::builder::estimate_overhead(ui.context, reasoning_enabled);
}

pub async fn handle_agent_event(
    event: AgentEvent,
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    slash: &SlashState,
    chain: &mut ChainState,
    #[cfg(feature = "loop")] validation_tx: &tokio::sync::mpsc::Sender<UserEvent>,
) -> anyhow::Result<()> {
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

            if run.response_start_block.is_none() {
                renderer.feed_mut().push_streaming_block(BlockStyle::Agent);
                run.response_start_block = Some(renderer.feed().block_count() - 1);
            }
            // Append the token to the running block: layout renders the
            // unfinished tail line as plain text and parses markdown only for
            // completed lines, instead of re-parsing the whole response.
            renderer.feed_mut().append_to_last(&safe);

            // Throttle repaints: redraw when a line completed (markdown
            // structure changes at line boundaries) or while the buffer is
            // small. The final full parse happens in handle_agent_done.
            if run.response_buf.len() >= 200 && !run.response_buf.ends_with('\n') {
                return Ok(());
            }

            renderer.render_viewport()?;
            run.agent_line_started = true;
        }
        AgentEvent::ToolCall { name, args } => {
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
            ui.session.add_tool_call(&name, &args);
            save_session_if_settled(ui.session, ui.cli, run, renderer)?;
            let line = format!(
                "◈ {}",
                crate::ui::utils::format_tool_call_summary(&name, &args)
            );
            renderer.write_line(&sanitize_output(&line), C_TOOL)?;
        }
        #[cfg(any(feature = "subagents", feature = "acp"))]
        AgentEvent::SubagentToolCall { name, args } => {
            ui.session.add_subagent_tool_call(&name, &args);
            save_session_if_settled(ui.session, ui.cli, run, renderer)?;
            let line = format!(
                "⌥ {}",
                crate::ui::utils::format_tool_call_summary(&name, &args)
            );
            renderer.write_line(&sanitize_output(&line), C_TOOL)?;
        }
        AgentEvent::ToolResult { name, output } => {
            let (_, artifact) = ui.session.add_tool_result_with_artifact(&name, &output);
            if let (Some(pending), Some(path)) = (run.pending_turn.as_mut(), artifact) {
                pending.record_tool_output(path);
            }
            save_session_if_settled(ui.session, ui.cli, run, renderer)?;
            if name == "todo_write" {
                let list = TODO_LIST.lock().unwrap_or_else(|e| e.into_inner());
                if list.is_empty() {
                    renderer.write_line("tasks cleared", Color::DarkGrey)?;
                } else {
                    let total = list.len();
                    let completed = list.iter().filter(|t| t.status == "completed").count();
                    renderer.write_line(
                        &format!("tasks  {} done / {} total", completed, total),
                        C_TOOL,
                    )?;
                    for item in list.iter() {
                        let icon = match item.status.as_str() {
                            "completed" => "[x]",
                            "in_progress" => "[>]",
                            "cancelled" => "[-]",
                            _ => "[ ]",
                        };
                        let status_color = match item.status.as_str() {
                            "completed" => Color::Green,
                            "in_progress" => C_TOOL,
                            "cancelled" => Color::DarkGrey,
                            _ => Color::DarkGrey,
                        };
                        let priority_mark = match item.priority.as_str() {
                            "high" => "!!",
                            "medium" => "! ",
                            _ => "  ",
                        };
                        renderer.write_line(
                            &format!("  {} {} {}", icon, priority_mark, item.content),
                            status_color,
                        )?;
                    }
                }
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
        AgentEvent::Done {
            response,
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
        } => {
            handle_agent_done(
                response,
                TurnUsage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    cache_creation_input_tokens,
                },
                renderer,
                run,
                ui,
                chain,
                #[cfg(feature = "loop")]
                validation_tx,
            )
            .await?;
        }
        AgentEvent::CompletionCall {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
        } => {
            // Real provider-reported usage for the call that just finished.
            // The local len()/4 heuristic in session.total_estimated_tokens
            // undercounts code-heavy turns; trust the real number as a floor
            // so the status bar's x/y/% reflects what the provider actually saw.
            // Use the cache-inclusive prompt size so Anthropic cache hits (which
            // report input_tokens excluding cached tokens) don't deflate it.
            let real = Session::real_input_tokens(
                ui.cfg.is_anthropic_native(&ui.session.provider),
                input_tokens,
                cached_input_tokens,
                cache_creation_input_tokens,
            )
            .saturating_add(output_tokens);
            if real > ui.session.total_estimated_tokens {
                ui.session.total_estimated_tokens = real;
            }
            // Accumulate cost for intermediate calls (tool-use turns). The Done
            // event only carries the final call's usage, so without this every
            // tool-call round-trip would go uncosted.
            ui.session.total_input_tokens =
                ui.session.total_input_tokens.saturating_add(input_tokens);
            ui.session.total_output_tokens =
                ui.session.total_output_tokens.saturating_add(output_tokens);
            ui.session.total_cost += crate::pricing::estimate_cost(
                crate::pricing::billable_input_tokens(
                    ui.cfg.is_anthropic_native(&ui.session.provider),
                    input_tokens,
                    cached_input_tokens,
                    cache_creation_input_tokens,
                ),
                output_tokens,
                ui.session.input_token_cost,
                ui.session.output_token_cost,
            );
        }
        AgentEvent::Retrying { attempt, max } => {
            run.was_reasoning = false;
            if run.agent_line_started {
                renderer.write_line("", Color::White)?;
                run.agent_line_started = false;
            }
            run.response_buf.clear();
            run.response_start_block = None;
            renderer.write_line(&format!("retrying... ({}/{})", attempt, max), Color::Yellow)?;
        }
        AgentEvent::Error(e) => {
            run.was_reasoning = false;
            run.is_running = false;
            if let Some(ss) = ui.status_signals.as_ref() {
                ss.send_stop();
            }
            run.agent_rx = None;
            run.agent_line_started = false;
            run.response_buf.clear();
            run.response_start_block = None;
            save_session_if_settled(ui.session, ui.cli, run, renderer)?;
            let safe = sanitize_output(&e);
            renderer.write_line(&format!("error: {}", safe), C_ERROR)?;
        }
    }
    Ok(())
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

async fn handle_agent_done(
    response: CompactString,
    usage: TurnUsage,
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    chain: &mut ChainState,
    #[cfg(feature = "loop")] validation_tx: &tokio::sync::mpsc::Sender<UserEvent>,
) -> anyhow::Result<()> {
    // `chain` is only read by the /loop-respawn and worktree-return paths.
    #[cfg(not(any(feature = "loop", feature = "git-worktree")))]
    let _ = &chain;
    run.was_reasoning = false;

    // Commit the provider's completed response and accounting before any
    // fallible presentation or post-processing. The App wrapper can then
    // persist this valid success even if rendering, compaction, validation
    // startup, or worktree-return presentation fails afterward.
    ui.session.add_message(MessageRole::Assistant, &response);
    ui.session.total_input_tokens = ui
        .session
        .total_input_tokens
        .saturating_add(usage.input_tokens);
    ui.session.total_output_tokens = ui
        .session
        .total_output_tokens
        .saturating_add(usage.output_tokens);
    ui.session.total_cost += crate::pricing::estimate_cost(
        crate::pricing::billable_input_tokens(
            ui.cfg.is_anthropic_native(&ui.session.provider),
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.cache_creation_input_tokens,
        ),
        usage.output_tokens,
        ui.session.input_token_cost,
        ui.session.output_token_cost,
    );
    let context_input_tokens = Session::real_input_tokens(
        ui.cfg.is_anthropic_native(&ui.session.provider),
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_creation_input_tokens,
    );
    ui.session
        .set_calibration(context_input_tokens, usage.output_tokens);

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
    let loop_running = chain.loop_state.as_ref().is_some_and(|ls| ls.active);
    #[cfg(not(feature = "loop"))]
    let loop_running = false;

    let qm = crate::config::quick_models_map(ui.cfg);

    #[cfg(feature = "memory")]
    let reserve = crate::extras::memory::effective_reserve(
        ui.cfg.resolve_reserve_tokens(&ui.session.model, &qm),
        ui.context.memory.as_deref(),
    );
    #[cfg(not(feature = "memory"))]
    let reserve = ui.cfg.resolve_reserve_tokens(&ui.session.model, &qm);

    if !loop_running
        && ui.cfg.resolve_compact_enabled()
        && ui.session.needs_compaction(reserve)
        && !ui.cli.no_session
    {
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
    run.agent_rx = None;

    #[cfg(feature = "loop")]
    if let Some(ls) = chain.loop_state.as_mut()
        && ls.active
    {
        let summary: String = response
            .chars()
            .take(crate::extras::r#loop::SUMMARY_TRUNCATION_CHARS)
            .collect();
        ls.last_summary = Some(summary.clone());

        if let Some(cmd) = ls.run_cmd.clone() {
            let operation = crate::extras::r#loop::validation::start(&ui.sandbox, &cmd);
            let operation_id = run.begin_validation(operation.cancellation());
            run.main_abort = None;
            // Keep semantic interrupt routing active while the validator runs,
            // but let the main event loop continue consuming `/btw` and keys.
            run.is_running = true;
            let validation_tx = validation_tx.clone();
            tokio::spawn(async move {
                let result = operation.wait().await;
                let _ = validation_tx
                    .send(UserEvent::LoopValidationDone(LoopValidationEvent {
                        operation_id,
                        response,
                        summary,
                        result,
                    }))
                    .await;
            });
            return Ok(());
        }

        finish_loop_iteration(response.as_str(), summary, None, renderer, run, ui, chain).await?;
    }

    #[cfg(feature = "git-worktree")]
    finish_worktree_return(renderer, run, ui, chain).await?;

    Ok(())
}

#[cfg(feature = "loop")]
pub(crate) async fn handle_loop_validation_event(
    event: LoopValidationEvent,
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    chain: &mut ChainState,
) -> anyhow::Result<bool> {
    if !run.complete_validation(event.operation_id) {
        return Ok(false);
    }
    if !chain.loop_state.as_ref().is_some_and(|state| state.active) {
        return Ok(true);
    }

    run.is_running = false;
    finish_loop_iteration(
        event.response.as_str(),
        event.summary,
        Some(event.result.render()),
        renderer,
        run,
        ui,
        chain,
    )
    .await?;

    #[cfg(feature = "git-worktree")]
    finish_worktree_return(renderer, run, ui, chain).await?;
    Ok(true)
}

#[cfg(feature = "loop")]
async fn finish_loop_iteration(
    response: &str,
    summary: String,
    validation_output: Option<String>,
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    chain: &mut ChainState,
) -> anyhow::Result<()> {
    let Some(ls) = chain.loop_state.as_mut() else {
        return Ok(());
    };
    if !ls.active {
        return Ok(());
    }
    ls.last_run_output = validation_output.clone();

    if let Err(error) = crate::extras::r#loop::transcript::save_iteration(
        &ui.session.id,
        ls.iteration,
        &ls.build_prompt(),
        response,
        validation_output.as_deref(),
        &summary,
    ) {
        renderer.write_line(
            &format!("warning: failed to save loop transcript: {error}"),
            C_ERROR,
        )?;
    }

    ls.iteration += 1;
    if ls.should_stop() {
        renderer.write_line(
            &format!(
                "[loop] max iterations ({}) reached, stopping",
                ls.iteration - 1
            ),
            C_AGENT,
        )?;
        ls.active = false;
        chain.loop_label = None;
        return Ok(());
    }

    let prompt = ls.build_prompt();
    run.agent = Some(
        ui.agent_build_ctx()
            .rebuild_agent(&ui.session.model, true)
            .await,
    );
    let runner = run
        .agent
        .as_ref()
        .expect("loop agent was rebuilt")
        .clone()
        .spawn_runner(
            prompt,
            Vec::new(),
            ui.cfg.retry.clone(),
            #[cfg(feature = "hooks")]
            Some(crate::extras::hooks::LoopInfo {
                iteration: ls.iteration,
                active: ls.active,
            }),
        )
        .await;
    run.agent_rx = Some(runner.event_rx);
    run.main_abort = Some(runner.abort_handle);
    run.is_running = true;
    if let Some(signals) = ui.status_signals.as_ref() {
        signals.send_start();
    }
    chain.loop_label = Some(ls.iteration_label());
    renderer.write_line(
        &format!("[loop] launching {}", ls.iteration_label()),
        C_AGENT,
    )?;
    Ok(())
}

#[cfg(feature = "git-worktree")]
async fn finish_worktree_return(
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    chain: &mut ChainState,
) -> anyhow::Result<()> {
    if let Some((main_path, wt_path, branch, force)) = chain.wt_return_path.take() {
        crate::extras::git_worktree::cleanup_worktree(&wt_path, &branch, &main_path, force);
        match std::env::set_current_dir(&main_path) {
            Ok(()) => {
                ui.session.working_dir = compact_str::CompactString::new(&main_path);
                ui.context.reload();
                apply_current_prompt_mode(ui.context, &ui.permission);
                run.agent = Some(
                    ui.agent_build_ctx()
                        .rebuild_agent(&ui.session.model, true)
                        .await,
                );
                crate::ui::events::render_session(
                    renderer, ui.session, ui.cli, ui.cfg, ui.context,
                )?;
                renderer.write_line(
                    &format!("merged and returned to main repo at {}", main_path),
                    C_AGENT,
                )?;
            }
            Err(e) => {
                renderer.write_line(
                    &format!("warning: failed to change back to main repo: {}", e),
                    C_ERROR,
                )?;
            }
        }
    }

    Ok(())
}

fn finalize_response_segment(
    renderer: &mut Renderer,
    run: &mut AgentRunState,
) -> anyhow::Result<()> {
    if run.response_buf.is_empty() {
        return Ok(());
    }

    if let Some(start) = run.response_start_block {
        // Drop anything interleaved after the streaming block, then finalize
        // the full segment (including its last line) as markdown.
        renderer.feed_mut().truncate_blocks(start + 1);
        renderer.feed_mut().finalize_last();
    } else {
        renderer
            .feed_mut()
            .push_block(BlockStyle::Agent, run.response_buf.as_str());
    }
    renderer.render_viewport()?;
    Ok(())
}
