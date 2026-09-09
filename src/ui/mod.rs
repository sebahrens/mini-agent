mod app;
#[cfg(test)]
pub(crate) use app::retire_scoped_task;
#[cfg(test)]
pub(crate) use app::{ClipboardShortcut, InterruptTarget, clipboard_shortcut, interrupt_target};
pub(crate) mod event_handler;
pub(crate) mod events;
pub(crate) mod feed;
pub(crate) mod input;
pub(crate) mod markdown;
mod permission_handler;
pub(crate) mod pickers;
pub(crate) mod prebuild;
pub(crate) mod renderer;
pub(crate) mod slash;
pub(crate) mod state;
pub(crate) mod statusline;
pub(crate) mod terminal;
pub(crate) mod utils;

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::style::Color;
use tokio::sync::mpsc;

#[cfg(feature = "mcp")]
use crate::config::Config;
use crate::context::ContextFiles;
use crate::event::UserEvent;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::SecurityMode;
use crate::permission::ask::AskReceiver;
use crate::permission::checker::PermCheck;
use crate::process_creation::StdCommandCreationExt;
use crate::provider::AnyAgent;
use crate::session::{MessageRole, Session};
use crate::ui::event_handler::ensure_agent;
#[cfg(feature = "advisor")]
use crate::ui::events::sanitize_output;
use crate::ui::input::InputEditor;
use crate::ui::renderer::Renderer;
use crate::ui::slash::handle_compress;
use crate::ui::state::{
    AgentRunState, BtwStats, ChainState, PendingMainTurn, SlashState, UiContext,
};

/// What [`apply_prompt_mode`] did with the prompt's `%%mode=` directive, so
/// callers can report the change without re-parsing the prompt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PromptModeOutcome {
    /// No directive, an unrecognized mode name, a directive that would raise
    /// the mode above the user's own selection, or no permission checker.
    None,
    /// `%%mode=last_user_mode`: the user-selected mode was restored.
    RestoredUserMode,
    /// `%%mode=<mode>`: the given security mode was applied.
    Applied(SecurityMode),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MainAgentOutcome {
    pub(crate) default_prompt: Option<String>,
    pub(crate) prompt_applied: bool,
    pub(crate) prompt_mode: PromptModeOutcome,
}

/// Select a persona for the main loop. If its frontmatter names a prompt mode,
/// activate that mode through the normal prompt path while keeping the
/// explicit persona authoritative over a conflicting `%%agent=` directive.
pub(crate) fn apply_main_agent(
    name: &str,
    context: &mut ContextFiles,
    permission: &Option<PermCheck>,
) -> Option<MainAgentOutcome> {
    let default_prompt = context.activate_agent(name)?;
    let (prompt_applied, prompt_mode) = match default_prompt.as_deref() {
        Some(prompt) if context.prompts.contains_key(prompt) => {
            let outcome = apply_prompt_mode(prompt, context, permission);
            context.current_agent_name = Some(name.to_string());
            context.current_agent_explicit = true;
            (true, outcome)
        }
        _ => (false, PromptModeOutcome::None),
    };
    Some(MainAgentOutcome {
        default_prompt,
        prompt_applied,
        prompt_mode,
    })
}

/// Select prompt `name` as the current prompt, compose its optional
/// `%%agent=` persona, and apply its optional `%%mode=` directive to the
/// permission checker. Header directives are stripped from the stored prompt
/// content. Unknown prompt names are a no-op.
pub(crate) fn apply_prompt_mode(
    name: &str,
    context: &mut ContextFiles,
    permission: &Option<PermCheck>,
) -> PromptModeOutcome {
    let Some(mode_directive) = context.activate_prompt(name) else {
        return PromptModeOutcome::None;
    };
    apply_mode_directive(mode_directive.as_deref(), permission)
}

/// Apply an already-parsed `%%mode=` directive to the permission checker.
fn apply_mode_directive(
    mode_directive: Option<&str>,
    permission: &Option<PermCheck>,
) -> PromptModeOutcome {
    let (Some(mode_str), Some(perm)) = (mode_directive, permission) else {
        return PromptModeOutcome::None;
    };
    let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
    if mode_str == "last_user_mode" {
        guard.restore_user_mode();
        PromptModeOutcome::RestoredUserMode
    } else if let Some(mode) = SecurityMode::from_str(mode_str) {
        // Downgrade-only: the checker refuses any directive that would widen
        // the user's CLI/config/`/mode` selection.
        if guard.set_prompt_mode(mode) {
            PromptModeOutcome::Applied(mode)
        } else {
            PromptModeOutcome::None
        }
    } else {
        PromptModeOutcome::None
    }
}

/// Re-apply the current prompt's `%%mode=` directive after a context reload
/// (which restores the raw, unstripped prompt content from disk).
#[cfg(feature = "git-worktree")]
pub(crate) fn apply_current_prompt_mode(
    context: &mut ContextFiles,
    permission: &Option<PermCheck>,
) {
    let Some(name) = context.current_prompt_name.clone() else {
        return;
    };
    let explicit_agent = context
        .current_agent_explicit
        .then(|| context.current_agent_name.clone());
    let mode_directive = context.activate_prompt(&name).flatten();
    if let Some(agent) = explicit_agent {
        context.current_agent_name =
            agent.filter(|name| context.agent_definitions.contains_key(name));
        context.current_agent_explicit = true;
    }
    apply_mode_directive(mode_directive.as_deref(), permission);
}

pub(super) const C_AGENT: Color = Color::White;
pub(super) const C_ERROR: Color = Color::Red;
pub(super) const C_TOOL: Color = Color::Yellow;
pub(super) const C_PERM: Color = Color::Magenta;
pub(super) const C_BTW: Color = Color::Cyan;
#[cfg(feature = "advisor")]
pub(super) const C_HANDOFF: Color = Color::Green;

pub(crate) fn refresh_display(
    renderer: &mut Renderer,
    input: &mut InputEditor,
    ui: &UiContext,
    run: &AgentRunState,
    chain: &ChainState,
    btw: BtwStats,
) -> io::Result<()> {
    // Reconcile the input height first so the chat viewport is drawn against
    // the size the input is about to occupy (avoids a stale separator when the
    // input shrinks, or chat text hidden under it when the input grows).
    renderer.sync_input_height(&input.buffer)?;
    renderer.render_viewport()?;
    let perm_mode = ui.permission.as_ref().map(|p| {
        p.lock()
            .unwrap_or_else(|e| e.into_inner())
            .mode()
            .to_string()
    });
    let statusline_ctx = crate::ui::statusline::StatusContext {
        workspace: ui.workspace.root(),
        loop_label: chain.loop_label.as_deref(),
        prompt_name: ui.context.current_prompt_name.as_deref(),
        perm_mode: perm_mode.as_deref(),
        chain_label: chain.label_msg.as_deref(),
        background_jobs: ui.sandbox.running_background_job_count(),
        btw_cost: btw.cost,
        btw_in: btw.input,
        btw_out: btw.output,
    };
    let statusline_key = crate::ui::statusline::cache_key(ui.session, &statusline_ctx);
    let statusline = renderer.cached_statusline(statusline_key, || {
        crate::ui::statusline::build(ui.session, &statusline_ctx)
    });
    renderer.draw_bottom(
        &input.buffer,
        input.cursor,
        &statusline,
        statusline_key,
        run.is_running,
    )?;
    if let Some(ref mut picker) = input.picker {
        let was_active = picker.active();
        picker.draw()?;
        if was_active {
            // The picker painted over the chat and bottom regions, which the
            // dirty-region tracking cannot see; force a full repaint next
            // frame so a closing picker never leaves remnants behind.
            renderer.invalidate();
        }
    }
    Ok(())
}

/// Idle cadence of the event thread's poll loop.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// How far ahead the event thread peeks after an `Enter`/`Ctrl+J` to decide
/// whether it is really a pasted newline, and how long it waits for the next
/// event before declaring a paste burst over. Terminals WITHOUT
/// bracketed-paste support (common on Windows conhost and some SSH/tmux
/// chains) deliver a multi-line paste as a rapid stream of key events whose
/// newlines arrive as `KeyCode::Enter` (`VK_RETURN` on Windows, `\r` on
/// Unix) or `Ctrl+J` (raw `\n` on Unix); without coalescing, each pasted
/// line would be submitted/queued separately or gain literal `j`s (#197).
/// 10 ms is far below the time a human needs to press another key after
/// `Enter`, so genuine submits are unaffected.
const PASTE_BURST_WINDOW: Duration = Duration::from_millis(10);

/// What a plain `Enter` keypress means in the current input stream.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnterVerdict {
    /// Genuine submit: no burst in progress and no input queued behind it.
    Submit,
    /// Pasted newline: mid-burst, or more input is already queued behind it.
    Newline,
}

/// Whether a key event is a candidate pasted newline: a bare `Enter`
/// (Windows conhost injects `VK_RETURN` records for pasted newlines, and
/// `\r` maps to `Enter` on Unix), or `Ctrl+J` — which is how crossterm
/// reports a raw pasted `\n` byte on Unix in raw mode (crossterm issue
/// #371). `Enter`/`j` with any other modifier combo is a deliberate key
/// combination and passes through untouched.
pub(crate) fn is_paste_newline_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    (code == KeyCode::Enter && modifiers == KeyModifiers::NONE)
        || (code == KeyCode::Char('j') && modifiers == KeyModifiers::CONTROL)
}

/// Paste-burst state for terminals without bracketed paste. A burst starts
/// when a paste-newline key (see [`is_paste_newline_key`]) has more input
/// queued right behind it, and ends once the input stream goes quiet for
/// [`PASTE_BURST_WINDOW`]. While a burst is active every such key is a pasted
/// newline, never a submit (and never a literal `j`) — so a multi-line paste
/// lands in the input buffer whole instead of submitting line by line (#197).
/// Pure state machine so it can be unit-tested without a terminal.
#[derive(Default)]
pub(crate) struct PasteBurst {
    active: bool,
}

impl PasteBurst {
    /// Poll timeout for the next event: short while a burst is alive so its
    /// end is detected quickly, idle cadence otherwise.
    pub(crate) fn wait_timeout(&self) -> Duration {
        if self.active {
            PASTE_BURST_WINDOW
        } else {
            IDLE_POLL
        }
    }

    /// No event arrived within the window: any burst in progress is over.
    pub(crate) fn on_timeout(&mut self) {
        self.active = false;
    }

    /// Classify a plain `Enter` press and update burst state.
    /// `more_input_pending` is the result of peeking [`PASTE_BURST_WINDOW`]
    /// ahead (callers skip the peek when a burst is already active).
    pub(crate) fn on_enter(&mut self, more_input_pending: bool) -> EnterVerdict {
        if self.active || more_input_pending {
            self.active = true;
            EnterVerdict::Newline
        } else {
            EnterVerdict::Submit
        }
    }
}

fn event_counts_as_paste_followup(event: &event::Event) -> bool {
    matches!(event, event::Event::Key(key) if key.kind == KeyEventKind::Press)
}

/// Peek through non-key terminal events without treating them as pasted text.
/// Every event read here is retained in order for normal dispatch.
fn queue_paste_followups(pending: &mut VecDeque<event::Event>) -> bool {
    let deadline = Instant::now() + PASTE_BURST_WINDOW;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        if !matches!(event::poll(remaining), Ok(true)) {
            return false;
        }
        let Ok(next) = event::read() else {
            return false;
        };
        let is_key_press = event_counts_as_paste_followup(&next);
        pending.push_back(next);
        if is_key_press {
            return true;
        }
    }
}

pub(crate) fn spawn_event_thread(
    user_tx: mpsc::Sender<UserEvent>,
    running: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut paste_burst = PasteBurst::default();
        let mut pending_events = VecDeque::new();
        while running.load(Ordering::Relaxed) {
            let next_event = if let Some(pending) = pending_events.pop_front() {
                Ok(pending)
            } else {
                let Ok(ready) = event::poll(paste_burst.wait_timeout()) else {
                    continue;
                };
                if !ready {
                    paste_burst.on_timeout();
                    continue;
                }
                event::read()
            };
            match next_event {
                Ok(event::Event::Key(key)) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    // A paste-newline key (bare Enter, or Ctrl+J — a raw
                    // pasted '\n' on Unix) is either a submit/normal key or
                    // — during a paste burst — a pasted newline (see
                    // PasteBurst). Enter/j with other modifiers passes
                    // through (Shift/Alt+Enter = literal newline).
                    let ev = if is_paste_newline_key(key.code, key.modifiers) {
                        let pending =
                            paste_burst.active || queue_paste_followups(&mut pending_events);
                        match paste_burst.on_enter(pending) {
                            EnterVerdict::Submit => UserEvent::Key(key),
                            // Reuse the paste path: inserts a literal '\n'
                            // into the input buffer.
                            EnterVerdict::Newline => UserEvent::Paste("\n".to_string()),
                        }
                    } else {
                        UserEvent::Key(key)
                    };
                    if user_tx.blocking_send(ev).is_err() {
                        break;
                    }
                }
                Ok(event::Event::Mouse(m)) => match m.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let scroll = if matches!(m.kind, MouseEventKind::ScrollUp) {
                            UserEvent::ScrollUp
                        } else {
                            UserEvent::ScrollDown
                        };
                        if user_tx.blocking_send(scroll).is_err() {
                            break;
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        let _ = user_tx.blocking_send(UserEvent::MouseDown {
                            row: m.row,
                            col: m.column,
                        });
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        let _ = user_tx.blocking_send(UserEvent::MouseDrag { row: m.row });
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        let _ = user_tx.blocking_send(UserEvent::MouseUp { row: m.row });
                    }
                    _ => {}
                },
                Ok(event::Event::Resize(_cols, _rows)) => {
                    let _ = user_tx.blocking_send(UserEvent::Resize);
                }
                Ok(event::Event::Paste(data)) => {
                    let _ = user_tx.blocking_send(UserEvent::Paste(data));
                }
                Err(_) => break,
                _ => {}
            }
        }
    })
}

/// Lazily initialise the MCP client manager (connects only on first use).
#[cfg(feature = "mcp")]
pub(crate) async fn ensure_mcp_manager<'a>(
    mcp: &'a mut Option<McpClientManager>,
    cfg: &'a Config,
    workspace: &std::sync::Arc<crate::paths::WorkspaceBinding>,
) -> Option<&'a McpClientManager> {
    if mcp.is_none()
        && let Some(servers) = &cfg.mcp_servers
    {
        *mcp = Some(McpClientManager::connect_all_in_binding(servers, workspace).await);
    }
    mcp.as_ref()
}

#[cfg(feature = "mcp")]
pub(crate) async fn rebind_mcp_manager(
    mcp: &mut Option<McpClientManager>,
    cfg: &Config,
    workspace: &std::sync::Arc<crate::paths::WorkspaceBinding>,
) {
    if let Some(previous) = mcp.take() {
        previous.shutdown().await;
    }
    if let Some(servers) = &cfg.mcp_servers {
        *mcp = Some(McpClientManager::connect_all_in_binding(servers, workspace).await);
    }
}

/// What to do with a submitted line, given whether a main run is already active.
/// Pure decision so it can be unit-tested without a TUI/agent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SubmitAction {
    /// Idle: start a run now.
    Run,
    /// Running + plain text: queue and replay after the current run finishes.
    Queue,
    /// Running + a command (`/`, `.`, `!`): can't queue meaningfully — tell the
    /// user to wait or Ctrl-C.
    RejectWhileRunning,
    /// Empty submit: ignore.
    Ignore,
}

/// Commands that are safe to run *even while a main run is active* because they
/// don't spawn or mutate the main run — the single "bypass" whitelist. Add
/// future parallel-safe commands here. Currently: `/queue` (queue management)
/// and `/btw` (isolated, tool-less side question on its own event stream).
pub(crate) fn allowed_while_running(text: &str) -> bool {
    let t = text.trim_start();
    t == "/queue" || t.starts_with("/queue ") || t == "/btw" || t.starts_with("/btw ")
}

/// Build the rewind picker's list of `(message_index, preview)` for every user
/// turn in the conversation, oldest first. Only user turns are offered: a rewind
/// lands just before a message you sent, dropping everything after it.
pub(crate) fn rewind_targets(session: &Session) -> Vec<(usize, String)> {
    session
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == MessageRole::User)
        .map(|(idx, m)| {
            let preview: String = m.content.chars().take(80).collect();
            (idx, preview.replace('\n', " "))
        })
        .collect()
}

pub(crate) fn classify_submission(is_running: bool, text: &str) -> SubmitAction {
    // Idle, or a whitelisted parallel-safe command → let it through to its
    // handler. Everything else, while running, is gated.
    if !is_running || allowed_while_running(text) {
        return SubmitAction::Run;
    }
    let t = text.trim_start();
    if t.is_empty() {
        SubmitAction::Ignore
    } else if t.starts_with('/') || t.starts_with('.') || t.starts_with('!') {
        SubmitAction::RejectWhileRunning
    } else {
        SubmitAction::Queue
    }
}

#[cfg(feature = "git-worktree")]
pub(crate) fn rebind_worktree_workspace(
    session: &mut Session,
    context: &mut ContextFiles,
    permission: &Option<crate::permission::checker::PermCheck>,
    active_workspace: &mut std::sync::Arc<crate::paths::WorkspaceBinding>,
    sandbox: &mut crate::sandbox::Sandbox,
    workspace: &std::path::Path,
    no_context_files: bool,
) -> anyhow::Result<()> {
    let replacement = std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(workspace)?);
    let replacement_sandbox = sandbox
        .clone()
        .rebind_workspace_binding(replacement.clone())
        .map_err(anyhow::Error::msg)?;
    if let Some(permission) = permission {
        permission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .rebind_working_dir(replacement.root())?;
    }

    session.working_dir = compact_str::CompactString::new(replacement.root().to_string_lossy());
    context.reload_from_binding(no_context_files, &replacement);
    #[cfg(feature = "hooks")]
    crate::extras::hooks::set_active_workspace(replacement.root());
    *sandbox = replacement_sandbox;
    *active_workspace = replacement;
    Ok(())
}

pub(crate) fn git_stash_in_workspace(
    workspace: &std::path::Path,
) -> std::io::Result<std::process::Output> {
    std::process::Command::new("git")
        .arg("stash")
        .current_dir(workspace)
        .output_guarded()
}

/// Result of a background agent prebuild.
#[cfg(feature = "mcp")]
pub(crate) type PrebuildPayload = (AnyAgent, Option<McpClientManager>);
#[cfg(not(feature = "mcp"))]
pub(crate) type PrebuildPayload = AnyAgent;

/// If the background prebuild hasn't delivered yet, block until it does.
#[cfg(feature = "mcp")]
pub(crate) async fn resolve_prebuild<'a>(
    agent: &'a mut Option<AnyAgent>,
    mcp_manager: &'a mut Option<McpClientManager>,
    prebuild_rx: &'a mut Option<mpsc::Receiver<PrebuildPayload>>,
) {
    if agent.is_some() {
        return;
    }
    if let Some(rx) = prebuild_rx.as_mut() {
        if let Some((a, mcp)) = rx.recv().await {
            *agent = Some(a);
            *mcp_manager = mcp;
        }
        *prebuild_rx = None;
    }
}

#[cfg(not(feature = "mcp"))]
pub(crate) async fn resolve_prebuild<'a>(
    agent: &'a mut Option<AnyAgent>,
    prebuild_rx: &'a mut Option<mpsc::Receiver<PrebuildPayload>>,
) {
    if agent.is_some() {
        return;
    }
    if let Some(rx) = prebuild_rx.as_mut() {
        if let Some(a) = rx.recv().await {
            *agent = Some(a);
        }
        *prebuild_rx = None;
    }
}

/// Starts a single main agent run for `text` and records its abort handle.
/// The ONLY place that sets `agent_rx`/`is_running` for user-driven runs, so the
/// "at most one main run" invariant is enforced in one spot. Callers must ensure
/// no run is already active (otherwise the previous one would be orphaned).
pub(crate) async fn start_main_run(
    text: &str,
    record_chat_history: bool,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    slash: &SlashState,
    prebuild_rx: &mut Option<mpsc::Receiver<PrebuildPayload>>,
) {
    #[allow(unused_mut)]
    let mut pending_turn = PendingMainTurn::capture(ui.session, text);
    #[cfg(feature = "memory")]
    if ui.context.refresh_memory_if_changed().await {
        // Any completed/racing prebuild contains the old system preamble.
        // Dropping the receiver also prevents a late stale result from being
        // installed after the fresh agent is built below.
        run.agent = None;
        *prebuild_rx = None;
    }
    // Wait for the background prebuild if it hasn't completed yet.
    #[cfg(feature = "mcp")]
    resolve_prebuild(&mut run.agent, &mut ui.mcp_manager, prebuild_rx).await;
    #[cfg(not(feature = "mcp"))]
    resolve_prebuild(&mut run.agent, prebuild_rx).await;

    ensure_agent(&mut run.agent, ui, slash.reasoning_enabled).await;
    run.request_tool_results_cleared = ui
        .session
        .tool_results_cleared_for_retention(ui.cfg.resolve_keep_recent_tool_results());
    let history = crate::agent::runner::convert_history_shared_with_tool_result_retention(
        ui.session,
        ui.cfg.resolve_keep_recent_tool_results(),
    );
    #[cfg(feature = "multimodal")]
    let history = {
        let media = pending_turn.take_pending_media(ui.session);
        if media.is_empty() {
            history
        } else {
            let mut h = history.to_vec();
            h.extend(crate::agent::runner::media_to_messages(media));
            h.into()
        }
    };
    let runner = run
        .agent
        .as_ref()
        .unwrap()
        .clone()
        .spawn_runner(
            text.to_string(),
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
    if let Some(ss) = ui.status_signals.as_ref() {
        ss.send_start();
    }
    if record_chat_history {
        record_started_main_turn(pending_turn, run, ui);
    } else {
        mark_main_turn_started(ui.session, run, pending_turn);
    }
}

pub(crate) fn mark_main_turn_started(
    session: &mut Session,
    run: &mut AgentRunState,
    pending_turn: PendingMainTurn,
) {
    session.add_message(MessageRole::User, pending_turn.prompt());
    run.pending_turn = Some(pending_turn);
}

/// Records the common bookkeeping for a main turn only after its runner has
/// started. Startup auto-triggers and editor-submitted turns must use the same
/// path so rollback and chat history cannot drift apart.
pub(crate) fn record_started_main_turn(
    pending_turn: PendingMainTurn,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
) {
    // Only user-authored starts enter global input history. Internal main-run
    // prompts (such as the worktree merge controller) call
    // mark_main_turn_started directly and deliberately remain ineligible.
    mark_main_turn_started(ui.session, run, pending_turn);
    run.pending_turn
        .as_mut()
        .expect("main turn was just recorded")
        .record_started(ui.session.updated_at.clone());
}

pub(crate) fn rollback_pending_main_turn(
    run: &mut AgentRunState,
    session: &mut Session,
) -> Option<String> {
    run.pending_turn.take().map(|turn| turn.rollback(session))
}

pub(crate) fn pending_main_turn_has_progress(run: &AgentRunState, session: &Session) -> bool {
    run.pending_turn
        .as_ref()
        .is_some_and(|turn| turn.has_progress(session, &run.response_buf, &run.turn_trace))
}

/// Make an interrupted turn protocol-complete before committing its partial
/// transcript. This is infallible so callers can use it on error-unwind and
/// Ctrl-C paths without risking a second rollback-triggering failure.
pub(crate) fn preserve_pending_main_turn_progress(
    run: &mut AgentRunState,
    session: &mut Session,
) -> bool {
    if !pending_main_turn_has_progress(run, session) {
        return false;
    }

    let needs_trace_recap = run
        .pending_turn
        .as_ref()
        .is_some_and(|turn| !turn.has_recorded_turn_messages(session))
        && run.response_buf.trim().is_empty()
        && !run.turn_trace.is_empty();
    if let Some(turn) = run.pending_turn.as_ref() {
        turn.finalize_unresolved_tool_calls(session);
    }
    if !run.response_buf.trim().is_empty() {
        session.add_message(MessageRole::Assistant, &run.response_buf);
    } else if needs_trace_recap {
        let recap = format!(
            "[Interrupted turn progress]\n{}",
            run.turn_trace
                .iter()
                .map(compact_str::CompactString::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        );
        session.add_message(MessageRole::Assistant, &recap);
    }
    true
}

/// Persist only a settled session. Tool events mutate the live transaction,
/// but the disk snapshot remains at the pre-turn state until the turn reaches
/// one terminal success/failure/cancellation transition.
pub(crate) fn persist_session_if_settled(
    session: &Session,
    persistence_enabled: bool,
    run: &AgentRunState,
) -> anyhow::Result<bool> {
    if !persistence_enabled || run.pending_turn.is_some() {
        return Ok(false);
    }
    crate::session::storage::save_session(session)?;
    Ok(true)
}

/// Continuation prompt injected after a mid-turn compaction. Hardcoded as a
/// `const` rather than a `prompts/*.md` file: every `.md` under `prompts/` is
/// loaded as a selectable mode, so a file here would pollute the prompt picker.
/// Acknowledging the compaction is deliberate — it frames the summary as "what
/// I already did," not as new user instructions. The narrow-tool-calls line is
/// always present because any mid-turn fire means the configured ceiling was
/// hit, so the urgency always applies.
const MID_TURN_CONTINUE_PROMPT: &str = "[Context was compacted to save space; \
the full prior history is in the system summary above.]\n\nContinue with the \
user's original task. Do not redo work already completed per the summary; focus \
on what remains. Context was tight, so prefer narrower follow-up tool calls over \
wide ones until pressure subsides.";

/// Mid-turn auto-compaction (PR H). Invoked when real provider prompt pressure
/// (`CompletionCall` usage / context window) crosses
/// `mid_turn_compact_threshold`, and only when `compact_enabled` is true.
///
/// The runner reaches this function only after it has correlated every
/// in-flight tool result and emitted the exact structured interaction prefix.
/// The UI has already persisted those ordered call/result records, so
/// compaction summarizes the canonical session transcript instead of a capped
/// presentation trace. No tool future is aborted to create this boundary.
pub(crate) async fn mid_turn_compact_and_respawn(
    pressure: f64,
    interactions: &[rig::message::Message],
    renderer: &mut Renderer,
    run: &mut AgentRunState,
    ui: &mut UiContext<'_>,
    slash: &SlashState,
) -> anyhow::Result<()> {
    // The runner sent the boundary event and returned voluntarily. Dropping
    // its handle is bookkeeping, not cancellation of a tool or provider call.
    run.main_abort.take();
    run.is_running = false;
    run.agent_rx = None;
    run.compaction_decision_tx = None;
    run.pending_compaction_pressure = None;
    run.was_reasoning = false;

    tracing::debug!(
        structured_interactions = interactions.len(),
        "compacting at a protocol-complete runner boundary"
    );

    // Tool calls and results are already exact structured session records.
    // Preserve only an outstanding prose segment; never synthesize a tool
    // recap from `turn_trace`.
    let mut recap = String::new();
    if !run.response_buf.trim().is_empty() {
        recap.push_str(run.response_buf.trim());
    }
    if let Some(todo_context) = ui.session.todos.critical_context() {
        if !recap.is_empty() {
            recap.push('\n');
        }
        recap.push_str(&todo_context);
        recap.push('\n');
    }
    let recap = recap.trim();
    if !recap.is_empty() {
        ui.session.add_message(MessageRole::Assistant, recap);
    }
    run.turn_trace.clear();
    run.response_buf.clear();
    run.response_start_block = None;
    run.agent_line_started = false;

    renderer.write_line(
        &format!(
            "mid-turn context relief, restarting (at {}%)...",
            (pressure * 100.0).round() as u64
        ),
        Color::DarkGrey,
    )?;

    // 3. Compact the session (no-op if its text history is under the limit).
    let compress_result =
        handle_compress(None, true, run, renderer, ui, slash.reasoning_enabled).await;
    if let Err(e) = compress_result {
        renderer.write_line(&format!("mid-turn compact error: {}", e), C_ERROR)?;
    }

    // 4. Respawn on the compacted history with the continuation prompt.
    ensure_agent(&mut run.agent, ui, slash.reasoning_enabled).await;
    run.request_tool_results_cleared = ui
        .session
        .tool_results_cleared_for_retention(ui.cfg.resolve_keep_recent_tool_results());
    let history = crate::agent::runner::convert_history_shared_with_tool_result_retention(
        ui.session,
        ui.cfg.resolve_keep_recent_tool_results(),
    );
    let runner = run
        .agent
        .as_ref()
        .unwrap()
        .clone()
        .spawn_runner(
            MID_TURN_CONTINUE_PROMPT.to_string(),
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
    if let Some(ss) = ui.status_signals.as_ref() {
        ss.send_start();
    }
    Ok(())
}

/// Hard stop for a turn whose context cannot be brought under the mid-turn
/// ceiling even after a compaction. What remains is the irreducible floor
/// (system prompt, tool schemas, kept-recent transcript, reserved response
/// space), so compacting again is futile. Aborts the run and shows the user the
/// full arithmetic — the model and context-window combination is simply too
/// small to run the agentic loop on this task.
pub(crate) fn stop_turn_context_exhausted(
    prompt_tokens: u64,
    threshold: f64,
    renderer: &mut Renderer,
    ui: &UiContext,
    run: &mut AgentRunState,
) -> anyhow::Result<()> {
    if let Some(h) = run.main_abort.take() {
        h.abort();
    }
    run.is_running = false;
    run.agent_rx = None;
    run.compaction_decision_tx = None;
    run.pending_compaction_pressure = None;
    run.was_reasoning = false;
    run.agent_line_started = false;
    run.turn_trace.clear();
    run.response_buf.clear();
    run.response_start_block = None;
    if let Some(ss) = ui.status_signals.as_ref() {
        ss.send_stop();
    }

    renderer.write_line("error: not enough context to continue this turn.", C_ERROR)?;
    renderer.write_line(
        "Compaction ran, but the next prompt was still over the mid-turn ceiling. \
         Compacting again cannot help: what remains is the irreducible floor (system \
         prompt, tool schemas, the kept-recent transcript, and reserved response \
         space). Stopping the turn so the conversation is not corrupted.",
        Color::White,
    )?;
    renderer.write_line("", Color::White)?;
    for line in context_exhausted_report(
        prompt_tokens,
        threshold,
        ui.session.context_window,
        ui.cfg.resolve_reserve_tokens(
            &ui.session.model,
            &crate::config::quick_models_map(ui.cfg),
            ui.session.context_window,
        ),
        ui.cfg.resolve_keep_recent_tokens(ui.session.context_window),
    ) {
        renderer.write_line(&line, Color::White)?;
    }
    Ok(())
}

/// Builds the math-and-guidance body for a context-exhaustion stop. Pure (no
/// I/O) so the arithmetic can be unit-tested. `window` must be non-zero (the
/// caller only reaches here after gating on `context_window > 0`).
pub(crate) fn context_exhausted_report(
    prompt_tokens: u64,
    threshold: f64,
    window: u64,
    reserve: u64,
    keep_recent: u64,
) -> Vec<String> {
    let ceiling = (threshold * window as f64) as u64;
    let pressure_pct = prompt_tokens as f64 / window as f64 * 100.0;
    let overflow = prompt_tokens.saturating_sub(ceiling);
    vec![
        format!("  context window .............. {window} tokens"),
        format!(
            "  mid-turn ceiling ............ {ceiling} tokens  ({:.0}% of window)",
            threshold * 100.0
        ),
        format!(
            "  prompt after compaction ..... {prompt_tokens} tokens  ({pressure_pct:.0}% of window)"
        ),
        format!("  overflow above ceiling ...... {overflow} tokens"),
        format!("  reserved for response ....... {reserve} tokens"),
        format!("  kept-recent budget .......... {keep_recent} tokens"),
        String::new(),
        "This model and context-window combination is too small to run zerostack's \
         agentic loop on this task. To proceed you can:"
            .to_string(),
        "  - increase context_window (and the model server's real KV cache) so the \
         window clears the floor above;"
            .to_string(),
        format!(
            "  - raise mid_turn_compact_threshold above {pressure_pct:.0}% so this prompt \
             fits under the ceiling (trades safety for room: the real KV cache must still \
             hold {prompt_tokens}+ tokens);"
        ),
        "  - lower keep_recent_tokens or reserve_tokens to shrink the floor;".to_string(),
        "  - switch to a model/server with a larger context window, or split the task \
         into smaller pieces."
            .to_string(),
    ]
}

pub async fn run_interactive(
    ui: UiContext<'_>,
    agent: Option<AnyAgent>,
    ask_rx: Option<AskReceiver>,
    auto_trigger_msg: Option<String>,
    #[cfg(feature = "advisor")] handoff_rx: Option<crate::extras::advisor::HandoffReceiver>,
    #[cfg(feature = "hooks")] session_start_task: Option<tokio::task::JoinHandle<()>>,
) -> anyhow::Result<()> {
    let mut app = app::App::new(
        ui,
        agent,
        ask_rx,
        auto_trigger_msg,
        #[cfg(feature = "advisor")]
        handoff_rx,
        #[cfg(feature = "hooks")]
        session_start_task,
    )
    .await?;
    let result = app.run().await;
    app.teardown().await;
    result
}

#[cfg(feature = "advisor")]
async fn read_handoff_response(
    user_rx: &mut mpsc::Receiver<UserEvent>,
    deferred: &mut VecDeque<UserEvent>,
    reply: &mut tokio::sync::oneshot::Sender<String>,
    mut render_preview: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<String> {
    let mut buffer = String::new();
    loop {
        let event = tokio::select! {
            biased;
            _ = reply.closed() => return Ok(String::new()),
            event = user_rx.recv() => match event {
                Some(event) => event,
                None => return Ok(String::new()),
            },
        };
        match event {
            UserEvent::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c' | 'C' | 'd' | 'D'))
                {
                    return Ok(String::new());
                }
                if !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) {
                    continue;
                }
                match key.code {
                    KeyCode::Enter => return Ok(buffer),
                    KeyCode::Esc => return Ok(String::new()),
                    KeyCode::Char(c) => buffer.push(c),
                    KeyCode::Backspace => {
                        buffer.pop();
                    }
                    _ => continue,
                }
            }
            UserEvent::Paste(text) => buffer.push_str(&text),
            event => {
                deferred.push_back(event);
                continue;
            }
        }
        render_preview(&sanitize_output(&buffer))?;
    }
}

#[cfg(feature = "advisor")]
pub(crate) async fn handle_human_handoff(
    mut req: crate::extras::advisor::HandoffRequest,
    renderer: &mut Renderer,
    user_rx: &mut mpsc::Receiver<UserEvent>,
    deferred: &mut VecDeque<UserEvent>,
    run: &mut AgentRunState,
) -> anyhow::Result<()> {
    if req.reply.is_closed() {
        return Ok(());
    }
    run.was_reasoning = false;
    if run.agent_line_started {
        renderer.write_line("", Color::White)?;
        run.agent_line_started = false;
    }

    renderer.write_line("[handoff] Model requests your guidance:", C_HANDOFF)?;
    for line in req.question.lines() {
        renderer.write_line(&format!("  | {}", sanitize_output(line)), C_HANDOFF)?;
    }
    renderer.write_line("", C_HANDOFF)?;
    renderer.write_line(
        "  Type or paste your response and press Enter (ESC or Ctrl-C to cancel):",
        C_HANDOFF,
    )?;

    let response = read_handoff_response(user_rx, deferred, &mut req.reply, |preview| {
        renderer.write_line(&format!("  > {preview}"), C_HANDOFF)
    })
    .await?;

    if response.is_empty() {
        renderer.write_line("  [cancelled]", C_HANDOFF)?;
    } else {
        renderer.write_line(
            &format!("  [sent: {}]", sanitize_output(&response)),
            C_HANDOFF,
        )?;
    }
    renderer.write_line("", Color::White)?;

    let _ = req.reply.send(response);
    Ok(())
}

#[cfg(all(test, feature = "advisor"))]
mod handoff_input_tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use tokio::sync::oneshot;

    fn key(code: KeyCode) -> UserEvent {
        UserEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[tokio::test]
    async fn editing_and_paste_preserve_exact_reply_and_defer_background_events() {
        let (tx, mut rx) = mpsc::channel(10);
        let (mut reply, reply_rx) = oneshot::channel();
        let pasted = "Привет\n\x1b[31mred\x1b[0m";
        for event in [
            UserEvent::Resize,
            UserEvent::Paste(pasted.into()),
            key(KeyCode::Char('界')),
            key(KeyCode::Backspace),
            UserEvent::LinkOpenFailed("open failed".into()),
            UserEvent::Key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT)),
            key(KeyCode::Enter),
            UserEvent::ScrollDown,
        ] {
            tx.send(event).await.unwrap();
        }
        drop(tx);
        let mut deferred = VecDeque::from([UserEvent::ScrollUp]);
        let mut previews = Vec::new();
        let response = read_handoff_response(&mut rx, &mut deferred, &mut reply, |text| {
            previews.push(text.to_owned());
            Ok(())
        })
        .await
        .unwrap();
        reply.send(response).unwrap();
        assert_eq!(reply_rx.await.unwrap(), format!("{pasted}Z"));
        assert_eq!(previews.last().unwrap(), "Привет\nredZ");
        assert!(previews.iter().all(|text| !text.contains('\x1b')));
        assert!(matches!(deferred.pop_front(), Some(UserEvent::ScrollUp)));
        assert!(matches!(deferred.pop_front(), Some(UserEvent::Resize)));
        assert!(
            matches!(deferred.pop_front(), Some(UserEvent::LinkOpenFailed(s)) if s == "open failed")
        );
        assert!(deferred.is_empty());
        assert!(matches!(rx.try_recv(), Ok(UserEvent::ScrollDown)));
    }

    #[tokio::test]
    async fn interrupts_cancel_and_unrelated_shortcuts_cannot_edit_or_submit() {
        let mut cases = vec![(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), "")];
        for c in ['c', 'C', 'd', 'D'] {
            for modifiers in [
                KeyModifiers::CONTROL,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ] {
                cases.push((KeyEvent::new(KeyCode::Char(c), modifiers), ""));
            }
        }
        for event in [
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            KeyEvent::new_with_kind(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
            KeyEvent::new_with_kind(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Repeat),
        ] {
            cases.push((event, "draft"));
        }
        for (event, expected) in cases {
            let (tx, mut rx) = mpsc::channel(4);
            let (mut reply, _reply_rx) = oneshot::channel();
            tx.send(UserEvent::Paste("draft".into())).await.unwrap();
            tx.send(UserEvent::Key(event)).await.unwrap();
            tx.send(key(KeyCode::Enter)).await.unwrap();
            tx.send(UserEvent::ScrollDown).await.unwrap();
            drop(tx);
            let mut deferred = VecDeque::new();
            assert_eq!(
                read_handoff_response(&mut rx, &mut deferred, &mut reply, |_| Ok(()))
                    .await
                    .unwrap(),
                expected,
                "{event:?}"
            );
            assert!(deferred.is_empty());
            if expected.is_empty() {
                assert!(matches!(rx.try_recv(), Ok(UserEvent::Key(k)) if k.code == KeyCode::Enter));
            }
            assert!(matches!(rx.try_recv(), Ok(UserEvent::ScrollDown)));
        }
    }

    #[tokio::test]
    async fn closed_input_cancels_partial_response_and_preview_errors_propagate() {
        for render_fails in [false, true] {
            let (tx, mut rx) = mpsc::channel(2);
            let (mut reply, _reply_rx) = oneshot::channel();
            tx.send(UserEvent::Resize).await.unwrap();
            tx.send(UserEvent::Paste("unfinished".into()))
                .await
                .unwrap();
            drop(tx);
            let mut deferred = VecDeque::new();
            let result = read_handoff_response(&mut rx, &mut deferred, &mut reply, |_| {
                if render_fails {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed display"))
                } else {
                    Ok(())
                }
            })
            .await;
            if render_fails {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
            } else {
                assert_eq!(result.unwrap(), "");
            }
            assert!(matches!(deferred.pop_front(), Some(UserEvent::Resize)));
            assert!(deferred.is_empty());
        }
    }

    #[tokio::test]
    async fn requester_cancellation_wakes_prompt_without_consuming_later_input() {
        for already_closed in [false, true] {
            let (tx, mut rx) = mpsc::channel(2);
            let (mut reply, reply_rx) = oneshot::channel();
            let mut receiver = Some(reply_rx);
            let mut deferred = VecDeque::new();
            if already_closed {
                drop(receiver.take());
            }
            {
                let response =
                    read_handoff_response(&mut rx, &mut deferred, &mut reply, |_| Ok(()));
                tokio::pin!(response);
                if !already_closed {
                    // Poll into a pending receive before cancelling the requesting task.
                    tokio::select! {
                        biased;
                        result = &mut response => panic!("live prompt finished early: {result:?}"),
                        _ = std::future::ready(()) => {}
                    }
                    drop(receiver.take());
                }
                tx.send(key(KeyCode::Char('n'))).await.unwrap();
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(1), &mut response)
                        .await
                        .expect("orphaned prompt must close")
                        .unwrap(),
                    ""
                );
            }
            assert!(matches!(rx.try_recv(), Ok(UserEvent::Key(k)) if k.code == KeyCode::Char('n')));
            assert!(deferred.is_empty());
        }
    }
}

#[cfg(test)]
mod paste_followup_tests {
    use super::*;

    #[test]
    fn only_key_presses_extend_an_unbracketed_paste_burst() {
        assert!(!event_counts_as_paste_followup(&event::Event::Resize(
            80, 24
        )));
        assert!(!event_counts_as_paste_followup(&event::Event::Paste(
            "payload".into()
        )));
        assert!(event_counts_as_paste_followup(&event::Event::Key(
            crossterm::event::KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
        )));
    }
}
