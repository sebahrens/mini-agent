use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Color;
use rig::message::Message;
use tokio::sync::mpsc;

use crate::config;
use crate::event::{AgentEvent, BtwEvent, UserEvent};
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::provider::AnyAgent;
use crate::sandbox::CommandCancellation;
use crate::sandbox::{
    CommandLimits, CommandStatus, DEFAULT_COMMAND_LIMITS, SupportCommandAudit, SupportCommandLimits,
};
use crate::session::{GitStatus, MessageRole, Session};
use crate::ui::event_handler;
use crate::ui::events::{render_session, sanitize_output};
use crate::ui::input::InputEditor;
use crate::ui::permission_handler::handle_permission_request;
use crate::ui::pickers::rewind::RewindOutcome;
use crate::ui::renderer::{
    self as renderer_mod, ChainPrompt, ClipboardCopyOutcome, Renderer, copy_to_clipboard,
    read_from_clipboard,
};
use crate::ui::slash::{apply_prompt_model, handle_compress, handle_slash};
use crate::ui::state::{
    AgentRunState, BtwStats, ChainState, PendingMainTurn, SlashState, UiContext,
};
use crate::ui::terminal::TerminalGuard;
use crate::ui::utils::{parse_color, to_ansi_256};

#[cfg(feature = "advisor")]
use super::handle_human_handoff;
#[cfg(all(feature = "mcp", feature = "git-worktree"))]
use super::rebind_mcp_manager;
use super::{
    C_AGENT, C_BTW, C_ERROR, C_TOOL, PrebuildPayload, apply_prompt_mode, classify_submission,
    mid_turn_compact_and_respawn, pending_main_turn_has_progress,
    preserve_pending_main_turn_progress, record_started_main_turn, refresh_display,
    rollback_pending_main_turn, spawn_event_thread, start_main_run, stop_turn_context_exhausted,
};
#[cfg(feature = "git-worktree")]
use super::{C_PERM, apply_current_prompt_mode};

const TURN_TRACE_MAX: usize = 64;
const BTW_MAX_INFLIGHT: usize = 4;

#[cfg(feature = "git-worktree")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreePromptChoice {
    Proceed,
    Abort,
}

#[cfg(feature = "git-worktree")]
async fn read_worktree_choice(
    user_rx: &mut mpsc::Receiver<UserEvent>,
    deferred: &mut std::collections::VecDeque<UserEvent>,
    proceed_key: char,
) -> WorktreePromptChoice {
    while let Some(event) = user_rx.recv().await {
        let UserEvent::Key(key) = event else {
            // Background completions and other input still belong to the main loop.
            deferred.push_back(event);
            continue;
        };
        if key.kind != crossterm::event::KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C' | 'd' | 'D'))
        {
            return WorktreePromptChoice::Abort;
        }
        if !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) {
            continue;
        }
        match key.code {
            KeyCode::Char('a' | 'A') | KeyCode::Enter | KeyCode::Esc => {
                return WorktreePromptChoice::Abort;
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&proceed_key) => {
                return WorktreePromptChoice::Proceed;
            }
            _ => {}
        }
    }
    WorktreePromptChoice::Abort
}

#[derive(Debug)]
struct GitStatusRefresh {
    working_dir: PathBuf,
    snapshot: Option<GitStatus>,
}

fn apply_git_status_refresh(
    session: &mut Session,
    workspace_root: &Path,
    refresh: GitStatusRefresh,
) -> bool {
    if refresh.working_dir != workspace_root {
        return false;
    }
    session.git_status = refresh.snapshot;
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MidTurnPressureAction {
    Ignore,
    ClearReliefLatch,
    Compact,
    StopContextExhausted,
}

fn mid_turn_pressure_action(
    awaiting_compaction_relief: bool,
    context_complete: bool,
    over_threshold: bool,
) -> MidTurnPressureAction {
    if !context_complete {
        MidTurnPressureAction::Ignore
    } else if !over_threshold {
        MidTurnPressureAction::ClearReliefLatch
    } else if awaiting_compaction_relief {
        MidTurnPressureAction::StopContextExhausted
    } else {
        MidTurnPressureAction::Compact
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptTarget {
    Btw,
    Validation,
    MainRun,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardShortcut {
    CopySelection,
    Paste,
}

pub(crate) fn clipboard_shortcut(
    key: KeyEvent,
    native_clipboard_available: bool,
) -> Option<ClipboardShortcut> {
    if native_clipboard_available
        && key.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT
        && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        Some(ClipboardShortcut::CopySelection)
    } else if native_clipboard_available
        && key.modifiers == KeyModifiers::CONTROL
        && matches!(key.code, KeyCode::Char('v' | 'V'))
    {
        Some(ClipboardShortcut::Paste)
    } else {
        None
    }
}

fn is_ctrl_h(key: KeyEvent) -> bool {
    (matches!(key.code, KeyCode::Char('h' | 'H')) && key.modifiers.contains(KeyModifiers::CONTROL))
        || key.code == KeyCode::Char('\u{8}')
}

#[cfg(test)]
mod ctrl_h_tests {
    use super::*;

    #[test]
    fn accepts_disambiguated_and_raw_control_h_without_arming_backspace() {
        assert!(is_ctrl_h(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::CONTROL
        )));
        assert!(is_ctrl_h(KeyEvent::new(
            KeyCode::Char('\u{8}'),
            KeyModifiers::NONE
        )));
        assert!(!is_ctrl_h(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE
        )));
    }
}

#[cfg(all(test, feature = "git-worktree"))]
mod worktree_choice_tests {
    use super::*;
    use crossterm::event::KeyEventKind;
    use std::collections::VecDeque;

    #[tokio::test]
    async fn explicit_choices_and_interrupts_apply_to_both_prompts() {
        for proceed_key in ['c', 'l'] {
            let proceed = KeyEvent::new(KeyCode::Char(proceed_key), KeyModifiers::NONE);
            let mut cases = vec![
                (proceed, WorktreePromptChoice::Proceed),
                (
                    KeyEvent::new(
                        KeyCode::Char(proceed_key.to_ascii_uppercase()),
                        KeyModifiers::SHIFT,
                    ),
                    WorktreePromptChoice::Proceed,
                ),
            ];
            for code in [
                KeyCode::Char('a'),
                KeyCode::Char('A'),
                KeyCode::Enter,
                KeyCode::Esc,
            ] {
                cases.push((
                    KeyEvent::new(code, KeyModifiers::NONE),
                    WorktreePromptChoice::Abort,
                ));
            }
            for c in ['c', 'C', 'd', 'D'] {
                for modifiers in [
                    KeyModifiers::CONTROL,
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ] {
                    cases.push((
                        KeyEvent::new(KeyCode::Char(c), modifiers),
                        WorktreePromptChoice::Abort,
                    ));
                }
            }
            for (key, expected) in cases {
                let (tx, mut rx) = mpsc::channel(2);
                tx.send(UserEvent::Key(key)).await.unwrap();
                // An opposite choice makes incorrectly ignoring the first key observable.
                let opposite = if expected == WorktreePromptChoice::Abort {
                    proceed
                } else {
                    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
                };
                tx.send(UserEvent::Key(opposite)).await.unwrap();
                drop(tx);
                let mut deferred = VecDeque::new();
                assert_eq!(
                    read_worktree_choice(&mut rx, &mut deferred, proceed_key).await,
                    expected,
                    "{proceed_key}: {key:?}"
                );
                assert!(deferred.is_empty());
                assert!(matches!(rx.try_recv(), Ok(UserEvent::Key(k)) if k == opposite));
            }
        }
    }

    #[tokio::test]
    async fn unrelated_modified_and_nonpress_keys_do_not_choose_an_action() {
        for proceed_key in ['c', 'l'] {
            let cases = [
                KeyEvent::new(KeyCode::Char(proceed_key), KeyModifiers::ALT),
                KeyEvent::new(KeyCode::Char(proceed_key), KeyModifiers::SUPER),
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                KeyEvent::new_with_kind(
                    KeyCode::Char(proceed_key),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ),
                KeyEvent::new_with_kind(
                    KeyCode::Char(proceed_key),
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat,
                ),
            ];
            for key in cases {
                // Both outcomes expose an accidental abort as well as an accidental proceed.
                for (expected, followup) in [
                    (WorktreePromptChoice::Abort, KeyCode::Esc),
                    (WorktreePromptChoice::Proceed, KeyCode::Char(proceed_key)),
                ] {
                    let (tx, mut rx) = mpsc::channel(2);
                    tx.send(UserEvent::Key(key)).await.unwrap();
                    tx.send(UserEvent::Key(KeyEvent::new(followup, KeyModifiers::NONE)))
                        .await
                        .unwrap();
                    drop(tx);
                    let mut deferred = VecDeque::new();
                    assert_eq!(
                        read_worktree_choice(&mut rx, &mut deferred, proceed_key).await,
                        expected,
                        "{proceed_key}: {key:?}"
                    );
                    assert!(deferred.is_empty());
                    assert!(matches!(
                        rx.try_recv(),
                        Err(mpsc::error::TryRecvError::Disconnected)
                    ));
                }
            }
        }
    }

    #[tokio::test]
    async fn background_events_keep_fifo_order_through_choice_or_eof() {
        for proceed_key in ['c', 'l'] {
            for choose in [false, true] {
                let (tx, mut rx) = mpsc::channel(5);
                let mut deferred = VecDeque::from([UserEvent::Paste("earlier".into())]);
                tx.send(UserEvent::Resize).await.unwrap();
                tx.send(UserEvent::LinkOpenFailed("opener failed".into()))
                    .await
                    .unwrap();
                tx.send(UserEvent::Paste("during prompt".into()))
                    .await
                    .unwrap();
                if choose {
                    tx.send(UserEvent::Key(KeyEvent::new(
                        KeyCode::Char(proceed_key),
                        KeyModifiers::NONE,
                    )))
                    .await
                    .unwrap();
                    tx.send(UserEvent::ScrollDown).await.unwrap();
                }
                drop(tx);
                assert_eq!(
                    read_worktree_choice(&mut rx, &mut deferred, proceed_key).await,
                    if choose {
                        WorktreePromptChoice::Proceed
                    } else {
                        WorktreePromptChoice::Abort
                    }
                );
                assert!(
                    matches!(deferred.pop_front(), Some(UserEvent::Paste(s)) if s == "earlier")
                );
                assert!(matches!(deferred.pop_front(), Some(UserEvent::Resize)));
                assert!(
                    matches!(deferred.pop_front(), Some(UserEvent::LinkOpenFailed(s)) if s == "opener failed")
                );
                assert!(
                    matches!(deferred.pop_front(), Some(UserEvent::Paste(s)) if s == "during prompt")
                );
                assert!(deferred.is_empty());
                if choose {
                    assert!(matches!(rx.try_recv(), Ok(UserEvent::ScrollDown)));
                }
                assert!(matches!(
                    rx.try_recv(),
                    Err(mpsc::error::TryRecvError::Disconnected)
                ));
            }
        }
    }
}

#[cfg(test)]
mod git_status_refresh_tests {
    use super::*;

    #[test]
    fn late_refresh_cannot_replace_status_after_a_workspace_switch() {
        let mut session = Session::new("openai", "model", 128_000, "");
        session.git_status = Some(GitStatus {
            modified: 1,
            ..GitStatus::default()
        });
        let cached = session.git_status.clone();

        assert!(!apply_git_status_refresh(
            &mut session,
            Path::new("/current"),
            GitStatusRefresh {
                working_dir: PathBuf::from("/previous"),
                snapshot: Some(GitStatus {
                    untracked: 9,
                    ..GitStatus::default()
                }),
            },
        ));
        assert_eq!(session.git_status, cached);

        let current = GitStatus {
            staged: 2,
            ..GitStatus::default()
        };
        assert!(apply_git_status_refresh(
            &mut session,
            Path::new("/current"),
            GitStatusRefresh {
                working_dir: PathBuf::from("/current"),
                snapshot: Some(current.clone()),
            },
        ));
        assert_eq!(session.git_status, Some(current));
    }
}

pub(crate) fn interrupt_target(
    btw_inflight: usize,
    validation_active: bool,
    main_running: bool,
) -> InterruptTarget {
    if btw_inflight > 0 {
        InterruptTarget::Btw
    } else if validation_active {
        InterruptTarget::Validation
    } else if main_running {
        InterruptTarget::MainRun
    } else {
        InterruptTarget::Exit
    }
}

pub(crate) struct App<'a> {
    ui: UiContext<'a>,
    run: AgentRunState,
    chain: ChainState,
    slash: SlashState,

    renderer: Renderer,
    input: InputEditor,
    last_branch_check: std::time::Instant,
    git_status_tx: mpsc::Sender<GitStatusRefresh>,
    git_status_rx: mpsc::Receiver<GitStatusRefresh>,
    git_status_in_flight: bool,
    ask_rx: Option<mpsc::Receiver<crate::permission::ask::AskRequest>>,
    #[cfg(feature = "advisor")]
    handoff_rx: Option<crate::extras::advisor::HandoffReceiver>,

    btw_tx: mpsc::Sender<BtwEvent>,
    btw_rx: mpsc::Receiver<BtwEvent>,
    btw_abort: Vec<(
        u32,
        tokio::task::AbortHandle,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<crate::agent::runner::AgentWorkScope>,
    )>,
    btw_inflight: usize,
    btw_next_id: u32,
    btw_total_cost: f64,
    btw_total_in: u64,
    btw_total_out: u64,

    user_tx: mpsc::Sender<UserEvent>,
    user_rx: mpsc::Receiver<UserEvent>,
    deferred_user_events: std::collections::VecDeque<UserEvent>,
    running: Arc<AtomicBool>,
    event_handle: Option<std::thread::JoinHandle<()>>,
    prebuild: Option<super::prebuild::AgentPrebuild>,
    terminal_guard: TerminalGuard,
}

impl<'a> App<'a> {
    pub(crate) async fn new(
        mut ui: UiContext<'a>,
        agent: Option<AnyAgent>,
        ask_rx: Option<mpsc::Receiver<crate::permission::ask::AskRequest>>,
        auto_trigger_msg: Option<String>,
        #[cfg(feature = "advisor")] handoff_rx: Option<crate::extras::advisor::HandoffReceiver>,
        #[cfg(feature = "hooks")] mut session_start_task: Option<tokio::task::JoinHandle<()>>,
    ) -> anyhow::Result<Self> {
        let terminal_guard = TerminalGuard::new()?;

        ui.session.show_cost_always = ui.cfg.resolve_show_cost_always();
        crate::ui::statusline::init(ui.cfg);

        ui.session.refresh_git_branch();
        let last_branch_check = std::time::Instant::now();

        let mut renderer = Renderer::new()?;
        renderer.set_statusline_height(crate::ui::statusline::line_count());
        renderer.set_monochrome(ui.cli.no_color);
        renderer.set_chat_margin(ui.cfg.resolve_chat_left_margin());
        if let Some(ref theme_name) = ui.context.current_theme_name {
            if let Some(content) = ui.context.themes.get(theme_name.as_str()) {
                crate::context::themes::apply(content, &mut renderer);
            }
        } else if let Some(colors) = &ui.cfg.colors {
            let chat_bg = colors.chat_background.as_deref().and_then(parse_color);
            let input_bg = colors.input_background.as_deref().and_then(parse_color);
            let status_bg = colors.status_background.as_deref().and_then(parse_color);
            if matches!(colors.scheme_type, config::SchemeType::Ansi) {
                renderer.set_background_colors(
                    chat_bg.map(to_ansi_256),
                    input_bg.map(to_ansi_256),
                    status_bg.map(to_ansi_256),
                );
            } else {
                renderer.set_background_colors(chat_bg, input_bg, status_bg);
            }
        }

        let mut input = InputEditor::new();
        input.set_monochrome(ui.cli.no_color);
        input.set_prompt_names(ui.context.prompts.keys().cloned().collect());
        input.set_agent_names(ui.context.agent_definitions.keys().cloned().collect());
        input.set_theme_names(ui.context.themes.keys().cloned().collect());
        if let Some(editor) = &ui.cfg.editor {
            input.set_editor(editor.clone());
        }
        input.set_quick_model_names(config::quick_models_map(ui.cfg).into_keys().collect());
        {
            let mut providers: Vec<String> =
                ["anthropic", "openai", "gemini", "openrouter", "ollama"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
            providers.extend(ui.cfg.custom_providers_map().keys().cloned());
            input.set_provider_names(providers);
        }
        input.load_global_history();

        let mut run = AgentRunState {
            agent,
            ..AgentRunState::default()
        };
        let chain = ChainState::default();
        let slash = SlashState {
            show_reasoning: ui.cfg.resolve_show_reasoning(),
            reasoning_enabled: true,
            todo_tools_enabled: false,
        };
        ui.session.reasoning_enabled = slash.reasoning_enabled;
        #[cfg(feature = "goal")]
        let goal_block =
            crate::extras::goal::prompt::goal_block(ui.session.goal_store.snapshot().as_ref());
        #[cfg(not(feature = "goal"))]
        let goal_block: Option<String> = None;
        ui.session.overhead_tokens = crate::agent::builder::estimate_overhead(
            ui.context,
            slash.reasoning_enabled,
            ui.cli,
            ui.cfg,
            &ui.sandbox,
            goal_block.as_deref(),
        );

        render_session(&mut renderer, ui.session, ui.cli, ui.cfg, ui.context)?;
        let marker_path = crate::paths::process_paths()
            .expect("startup must initialize application paths")
            .welcome_marker_file();
        if ui.cfg.resolve_always_show_welcome() || !marker_path.exists() {
            crate::ui::events::show_welcome(&mut renderer)?;
            if !ui.cfg.resolve_always_show_welcome()
                && !crate::paths::artifact_disabled("welcome marker")
                && let Err(e) = crate::session::storage::atomic_write(&marker_path, "")
            {
                tracing::warn!("failed to write welcome marker (welcome will show again): {e}");
            }
        }
        refresh_display(
            &mut renderer,
            &mut input,
            &ui,
            &run,
            &chain,
            BtwStats::default(),
        )?;

        {
            let provider = ui.session.provider.to_string();
            let is_custom = ui.cfg.custom_providers_map().contains_key(&provider);
            let ids = crate::ui::slash::warm_model_cache(
                &provider, is_custom, &ui.client, ui.cli, ui.cfg,
            )
            .await;
            input.set_live_model_names(ids);
        }

        #[cfg(feature = "git-worktree")]
        if let Some(name) = &ui.cli.worktree {
            let wt_base_dir = ui.cli.resolve_wt_base_dir(ui.cfg);
            match crate::extras::git_worktree::create(
                ui.workspace.root(),
                name,
                wt_base_dir.as_deref(),
            )
            .await
            {
                Ok((path, _info)) => {
                    super::rebind_worktree_workspace(
                        ui.session,
                        ui.context,
                        &ui.permission,
                        &mut ui.workspace,
                        &mut ui.sandbox,
                        &path,
                        ui.cli.resolve_no_context_files(ui.cfg),
                    )?;
                    apply_current_prompt_mode(ui.context, &ui.permission);
                    #[cfg(feature = "mcp")]
                    rebind_mcp_manager(&mut ui.mcp_manager, ui.cfg, &ui.workspace).await;
                    run.agent = Some(
                        ui.agent_build_ctx()
                            .rebuild_agent(&ui.session.model, slash.reasoning_enabled)
                            .await,
                    );
                    if let Err(e) =
                        render_session(&mut renderer, ui.session, ui.cli, ui.cfg, ui.context)
                    {
                        tracing::warn!("failed to re-render session after worktree switch: {e}");
                    }
                }
                Err(e) => {
                    let _ = renderer.write_line(&format!("worktree failed: {}", e), C_ERROR);
                }
            }
        }
        #[cfg(feature = "git-worktree")]
        if ui.cli.parallel {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name = ts.to_string();
            let wt_base_dir = ui.cli.resolve_wt_base_dir(ui.cfg);
            match crate::extras::git_worktree::create(
                ui.workspace.root(),
                &name,
                wt_base_dir.as_deref(),
            )
            .await
            {
                Ok((path, _info)) => {
                    super::rebind_worktree_workspace(
                        ui.session,
                        ui.context,
                        &ui.permission,
                        &mut ui.workspace,
                        &mut ui.sandbox,
                        &path,
                        ui.cli.resolve_no_context_files(ui.cfg),
                    )?;
                    apply_current_prompt_mode(ui.context, &ui.permission);
                    #[cfg(feature = "mcp")]
                    rebind_mcp_manager(&mut ui.mcp_manager, ui.cfg, &ui.workspace).await;
                    run.agent = Some(
                        ui.agent_build_ctx()
                            .rebuild_agent(&ui.session.model, slash.reasoning_enabled)
                            .await,
                    );
                    if let Err(e) =
                        render_session(&mut renderer, ui.session, ui.cli, ui.cfg, ui.context)
                    {
                        tracing::warn!("failed to re-render session after worktree switch: {e}");
                    }
                }
                Err(e) => {
                    let _ = renderer.write_line(&format!("worktree failed: {}", e), C_ERROR);
                }
            }
        }

        #[cfg(feature = "hooks")]
        if auto_trigger_msg.is_some()
            && let Some(task) = session_start_task.take()
        {
            let _ = task.await;
        }

        if let Some(ref trigger_msg) = auto_trigger_msg {
            for line in trigger_msg.lines() {
                let safe_line = sanitize_output(line);
                renderer.write_line(&format!("> {}", safe_line), Color::Green)?;
            }
            renderer.write_line("", Color::White)?;

            event_handler::ensure_agent(&mut run.agent, &mut ui, slash.reasoning_enabled).await;
            run.request_tool_results_cleared = ui
                .session
                .tool_results_cleared_for_retention(ui.cfg.resolve_keep_recent_tool_results());
            let initial_turn = AutoTriggerTurn::prepare(
                ui.session,
                trigger_msg,
                ui.cfg.resolve_keep_recent_tool_results(),
            );
            let (prompt, history, pending_turn) = initial_turn.into_runner_inputs();
            let runner = run
                .agent
                .as_ref()
                .unwrap()
                .clone()
                .spawn_runner(
                    prompt,
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
            record_started_main_turn(pending_turn, &mut run, &mut ui);
        }

        let (user_tx, user_rx) = mpsc::channel::<UserEvent>(64);
        let running = Arc::new(AtomicBool::new(true));
        let event_handle = Some(spawn_event_thread(user_tx.clone(), running.clone()));

        let prebuild = if auto_trigger_msg.is_none() && run.agent.is_none() {
            let client_clone = ui.client.clone();
            let session_model = ui.session.model.to_string();
            let tool_output_session_id = ui.session.id.to_string();
            let tool_result_spills = ui.session.tool_result_spills.clone();
            #[cfg(feature = "js")]
            let js_session_state = ui.session.js_session_state.clone();
            let cli_clone = ui.cli.clone();
            let cfg_clone = ui.cfg.clone();
            let context_clone = ui.context.clone();
            let workspace_clone = ui.workspace.clone();
            let permission_clone = ui.permission.clone();
            let ask_tx_clone = ui.ask_tx.clone();
            let sandbox_clone = ui.sandbox.clone();
            let read_tracker_clone = ui.session.read_tracker.clone();
            let todo_store_clone = ui.session.todos.clone();
            #[cfg(feature = "goal")]
            let goal_store_clone = ui.session.goal_store.clone();
            #[cfg(feature = "skills")]
            let skill_services_clone = ui.skill_services.clone();
            let reasoning_enabled = slash.reasoning_enabled;
            let prebuild_scope = crate::agent::runner::AgentWorkScope::new();
            let prebuild = super::prebuild::AgentPrebuild::start(
                prebuild_scope,
                ui.prebuild_invalidated.clone(),
                async move {
                    #[cfg(feature = "mcp")]
                    let mcp = if !cli_clone.mcp_is_eligible(&cfg_clone) {
                        None
                    } else if let Some(ref servers) = cfg_clone.mcp_servers {
                        if !servers.is_empty() {
                            Some(
                                McpClientManager::connect_all_in_binding(servers, &workspace_clone)
                                    .await,
                            )
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let a = crate::ui::state::AgentBuildCtx {
                        prebuild_invalidated: None,
                        cli: &cli_clone,
                        cfg: &cfg_clone,
                        context: &context_clone,
                        workspace: &workspace_clone,
                        client: &client_clone,
                        permission: &permission_clone,
                        ask_tx: &ask_tx_clone,
                        sandbox: &sandbox_clone,
                        read_tracker: &read_tracker_clone,
                        todo_store: &todo_store_clone,
                        #[cfg(feature = "goal")]
                        goal_store: &goal_store_clone,
                        tool_output_session_id: &tool_output_session_id,
                        tool_result_spills: &tool_result_spills,
                        #[cfg(feature = "js")]
                        js_session_state: &js_session_state,
                        #[cfg(feature = "skills")]
                        skill_services: &skill_services_clone,
                        #[cfg(feature = "mcp")]
                        mcp_manager: mcp.as_ref(),
                    }
                    .rebuild_agent(&session_model, reasoning_enabled)
                    .await;

                    #[cfg(feature = "mcp")]
                    {
                        (a, mcp)
                    }
                    #[cfg(not(feature = "mcp"))]
                    {
                        a
                    }
                },
            );
            Some(prebuild)
        } else {
            None
        };

        #[cfg(feature = "hooks")]
        if let Some(task) = session_start_task.take() {
            let _ = task.await;
        }

        let (btw_tx, btw_rx) = mpsc::channel::<BtwEvent>(32);
        let (git_status_tx, git_status_rx) = mpsc::channel(1);

        let mut app = Self {
            ui,
            run,
            chain,
            slash,
            renderer,
            input,
            last_branch_check,
            git_status_tx,
            git_status_rx,
            git_status_in_flight: false,
            ask_rx,
            #[cfg(feature = "advisor")]
            handoff_rx,
            btw_tx,
            btw_rx,
            btw_abort: Vec::new(),
            btw_inflight: 0,
            btw_next_id: 0,
            btw_total_cost: 0.0,
            btw_total_in: 0,
            btw_total_out: 0,
            user_tx,
            user_rx,
            deferred_user_events: std::collections::VecDeque::new(),
            running,
            event_handle,
            prebuild,
            terminal_guard,
        };
        app.request_git_status_refresh();
        Ok(app)
    }

    fn request_git_status_refresh(&mut self) {
        if self.git_status_in_flight || !crate::ui::statusline::needs_git_status() {
            return;
        }

        let working_dir = self.ui.workspace.root().to_path_buf();
        let tx = self.git_status_tx.clone();
        self.git_status_in_flight = true;
        tokio::spawn(async move {
            let snapshot = Session::detect_git_status(&working_dir).await;
            let _ = tx
                .send(GitStatusRefresh {
                    working_dir,
                    snapshot,
                })
                .await;
        });
    }

    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        let result = self.run_inner().await;
        if result.is_err() && self.run.pending_turn.is_some() {
            self.fail_pending_main_turn().await;
        }
        result
    }

    async fn run_inner(&mut self) -> anyhow::Result<()> {
        loop {
            // Every adoption/rebuild path returns here, including a prompt that
            // awaited prebuild and deferred events that immediately continue.
            #[cfg(feature = "mcp")]
            if let Some(manager) = self.ui.mcp_manager.as_mut() {
                for notice in manager.take_notices() {
                    self.renderer
                        .write_line(&sanitize_output(&notice), C_ERROR)?;
                }
            }
            if let Some(event) = self.deferred_user_events.pop_front() {
                match self.handle_user_event(event).await? {
                    ControlFlow::Break(()) => break,
                    ControlFlow::Continue(()) => continue,
                }
            }
            self.ui.session.reasoning_enabled = self.slash.reasoning_enabled;
            if self.last_branch_check.elapsed() >= Duration::from_secs(1) {
                self.ui.session.refresh_git_branch();
                self.request_git_status_refresh();
                self.last_branch_check = std::time::Instant::now();
            }

            tokio::select! {
                Some(refresh) = self.git_status_rx.recv() => {
                    self.git_status_in_flight = false;
                    if apply_git_status_refresh(self.ui.session, self.ui.workspace.root(), refresh) {
                        self.refresh()?;
                    } else {
                        self.request_git_status_refresh();
                    }
                }
                Some(ev) = self.user_rx.recv() => {
                    match self.handle_user_event(ev).await? {
                        ControlFlow::Break(()) => break,
                        ControlFlow::Continue(()) => {}
                    }
                }
                Some(prebuilt) = async { self.prebuild.as_mut()?.recv().await }, if !self.run.is_running => {
                    self.take_prebuild(prebuilt).await;
                    self.refresh()?;
                }
                Some(event) = async { self.run.agent_rx.as_mut()?.recv().await } => {
                    self.handle_agent_event(event).await?;
                }
                Some(ask_req) = async { self.ask_rx.as_mut()?.recv().await } => {
                    // A goal's time budget measures how long the agent worked,
                    // not how long the user took to answer.
                    #[cfg(feature = "goal")]
                    if let Some(collector) = self.run.goal_round.as_mut() {
                        collector.pause_clock();
                    }
                    handle_permission_request(
                        ask_req,
                        &mut self.renderer,
                        &mut self.ui,
                        &mut self.run,
                        &mut self.user_rx,
                        &mut self.deferred_user_events,
                    ).await?;
                    #[cfg(feature = "goal")]
                    if let Some(collector) = self.run.goal_round.as_mut() {
                        collector.resume_clock();
                    }
                    self.refresh()?;
                }
                Some(bev) = self.btw_rx.recv() => {
                    self.handle_btw_event(bev)?;
                    self.refresh()?;
                }
                _ = tokio::time::sleep(Duration::from_millis(100)), if self.run.is_running => {
                    self.renderer.tick_spinner()?;
                }
                else => {
                    if let Some(rx) = self.prebuild.as_mut()
                        && !self.run.is_running
                        && let Some(payload) = rx.try_recv()
                    {
                        self.take_prebuild(payload).await;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }

            #[cfg(feature = "advisor")]
            if let Some(ref mut rx) = self.handoff_rx
                && let Ok(req) = rx.try_recv()
            {
                handle_human_handoff(
                    req,
                    &mut self.renderer,
                    &mut self.user_rx,
                    &mut self.deferred_user_events,
                    &mut self.run,
                )
                .await?;
                self.refresh()?;
            }
        }

        self.handle_worktree_auto_merge().await?;
        Ok(())
    }

    pub(crate) async fn teardown(mut self) {
        self.pause_event_thread();

        // Retire owned work before closing the services it can still use.
        const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);
        for (_, abort, task, scope) in self.btw_abort.drain(..) {
            abort.abort();
            if let Err(error) =
                retire_scoped_task(task, scope, "side-question", TEARDOWN_TIMEOUT).await
            {
                tracing::warn!(%error, "TUI side-question cleanup failed");
            }
        }

        if let Err(error) = self.run.retire(TEARDOWN_TIMEOUT).await {
            tracing::warn!(%error, "TUI main-run cleanup failed");
        }
        if let Some(prebuild) = self.prebuild.take()
            && let Err(error) = prebuild.retire(TEARDOWN_TIMEOUT).await
        {
            tracing::warn!(%error, "TUI prebuild cleanup failed");
        }

        #[cfg(feature = "lsp")]
        crate::extras::lsp::shutdown_live_managers().await;
        #[cfg(feature = "mcp")]
        if let Some(mgr) = self.ui.mcp_manager {
            mgr.shutdown().await;
        }
    }

    fn refresh(&mut self) -> io::Result<()> {
        refresh_display(
            &mut self.renderer,
            &mut self.input,
            &self.ui,
            &self.run,
            &self.chain,
            BtwStats {
                cost: self.btw_total_cost,
                input: self.btw_total_in,
                output: self.btw_total_out,
            },
        )
    }

    async fn handle_user_event(&mut self, ev: UserEvent) -> anyhow::Result<ControlFlow<(), ()>> {
        match ev {
            UserEvent::Resize => {
                self.renderer.resize();
            }
            UserEvent::ScrollUp => {
                if !self.renderer.input_scroll_up() {
                    self.renderer.scroll_line_up();
                }
            }
            UserEvent::ScrollDown => {
                if self.renderer.is_scrolling() {
                    self.renderer.scroll_line_down();
                } else {
                    self.renderer.input_scroll_down();
                }
            }
            UserEvent::MouseDown { row, col } => {
                if let Some(pos) =
                    self.renderer
                        .input_cursor_for_click(row, col, &self.input.buffer)
                {
                    self.input.set_cursor(pos);
                } else if row < self.renderer.visible_lines() as u16
                    && let Some(idx) = self.renderer.buffer_line_at_row(row)
                {
                    if let Some(url) = self.renderer.link_url_at(idx, col) {
                        let tx = self.user_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = renderer_mod::open_url(&url).await {
                                let _ = tx.send(UserEvent::LinkOpenFailed(error.to_string())).await;
                            }
                        });
                    } else {
                        self.renderer.selection_active = true;
                        self.renderer.selection_start = Some(idx);
                        self.renderer.selection_end = Some(idx);
                    }
                }
            }
            UserEvent::MouseDrag { row } => {
                if self.renderer.selection_active
                    && let Some(idx) = self.renderer.buffer_line_at_row(row)
                {
                    self.renderer.selection_end = Some(idx);
                }
            }
            UserEvent::MouseUp { row } => {
                if self.renderer.selection_active {
                    if let Some(idx) = self.renderer.buffer_line_at_row(row) {
                        self.renderer.selection_end = Some(idx);
                    }
                    self.copy_selection_to_clipboard().await?;
                }
            }
            UserEvent::LinkOpenFailed(error) => {
                self.renderer
                    .write_line(&format!("cannot open link: {error}"), C_ERROR)?;
            }
            UserEvent::Paste(data) => {
                self.input.handle_paste(data);
            }
            #[cfg(feature = "loop")]
            UserEvent::LoopValidationDone(event) => {
                self.handle_loop_validation_event(event).await?;
            }
            #[cfg(feature = "mcp")]
            UserEvent::McpLoginDone { server, error } => {
                self.handle_mcp_login_done(server, error).await?;
            }
            UserEvent::Key(key) => {
                match clipboard_shortcut(key, cfg!(windows)) {
                    Some(ClipboardShortcut::CopySelection) => {
                        self.copy_selection_to_clipboard().await?;
                        self.refresh()?;
                        return Ok(ControlFlow::Continue(()));
                    }
                    Some(ClipboardShortcut::Paste) => {
                        match read_from_clipboard() {
                            Ok(text) => self.input.handle_paste(text),
                            Err(error) => self.renderer.write_line(
                                &format!("paste from clipboard failed: {error}"),
                                C_ERROR,
                            )?,
                        }
                        self.refresh()?;
                        return Ok(ControlFlow::Continue(()));
                    }
                    None => {}
                }
                let is_ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                let is_ctrl_d =
                    key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL);
                if is_ctrl_c || is_ctrl_d {
                    #[cfg(feature = "loop")]
                    let validation_active = self.run.validation_active();
                    #[cfg(not(feature = "loop"))]
                    let validation_active = false;
                    match interrupt_target(
                        self.btw_inflight,
                        validation_active,
                        self.run.is_running,
                    ) {
                        InterruptTarget::Btw => {
                            for (_, _, task, scope) in self.btw_abort.drain(..) {
                                retire_scoped_task(
                                    task,
                                    scope,
                                    "side-question",
                                    std::time::Duration::from_secs(5),
                                )
                                .await?;
                            }
                            self.btw_inflight = 0;
                            self.renderer.write_line("btw cancelled", C_ERROR)?;
                        }
                        InterruptTarget::Validation | InterruptTarget::MainRun => {
                            self.abort_main_run().await?;
                        }
                        InterruptTarget::Exit => return Ok(ControlFlow::Break(())),
                    }
                    self.refresh()?;
                    return Ok(ControlFlow::Continue(()));
                }

                if let Err(e) = self.handle_key_event(key).await {
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::Interrupted)
                    {
                        return Ok(ControlFlow::Break(()));
                    }
                    return Err(e);
                }
            }
        }

        self.refresh()?;
        Ok(ControlFlow::Continue(()))
    }

    async fn copy_selection_to_clipboard(&mut self) -> anyhow::Result<()> {
        let Some(text) = self.renderer.selected_text() else {
            self.renderer.clear_selection();
            return Ok(());
        };
        match copy_to_clipboard(&text).await {
            Ok(ClipboardCopyOutcome::Confirmed) => {
                self.renderer.write_line("copied selection", Color::Green)?;
                self.renderer.clear_selection();
            }
            Ok(ClipboardCopyOutcome::FallbackRequested) => {
                self.renderer
                    .write_line("copy requested through terminal", Color::Green)?;
                self.renderer.clear_selection();
            }
            Err(error) => {
                self.renderer
                    .write_line(&format!("copy to clipboard failed: {error}"), C_ERROR)?;
                self.renderer.clear_selection();
            }
        }
        Ok(())
    }

    async fn handle_key_event(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        if self.renderer.selection_active && key.code == KeyCode::Char('y') {
            self.copy_selection_to_clipboard().await?;
            return Ok(());
        }
        if self.renderer.selection_active && key.code == KeyCode::Esc {
            self.renderer.clear_selection();
            return Ok(());
        }

        let ctrl_r =
            key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl_r {
            self.slash.show_reasoning = !self.slash.show_reasoning;
            self.renderer.write_line(
                &format!(
                    "reasoning visibility: {}",
                    if self.slash.show_reasoning {
                        "on"
                    } else {
                        "off"
                    }
                ),
                Color::White,
            )?;
            return Ok(());
        }

        match key.code {
            KeyCode::PageUp => {
                self.renderer.scroll_page_up();
                return Ok(());
            }
            KeyCode::PageDown => {
                self.renderer.scroll_page_down();
                return Ok(());
            }
            KeyCode::Home => {
                self.renderer.scroll_to_top();
                return Ok(());
            }
            KeyCode::End => {
                self.renderer.scroll_to_bottom()?;
                return Ok(());
            }
            _ => {}
        }

        if self.input.picker.as_ref().is_some_and(|p| p.active())
            && self.input.handle_picker_key(key)
        {
            if let Some(RewindOutcome::Confirmed(idx)) = self.input.take_rewind_outcome() {
                let text = self
                    .ui
                    .session
                    .messages
                    .get(idx)
                    .map(|m| m.content.to_string());
                if self.ui.session.rewind_to(idx) > 0 {
                    if let Some(text) = text {
                        self.input.load_text(&text);
                    }
                    self.save_session()?;
                    render_session(
                        &mut self.renderer,
                        self.ui.session,
                        self.ui.cli,
                        self.ui.cfg,
                        self.ui.context,
                    )?;
                    self.renderer
                        .write_line("rewound; /redo to restore", Color::Green)?;
                }
            }
            return Ok(());
        }

        if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.pause_event_thread();
            let result = self.input.open_in_editor(&mut self.terminal_guard);
            self.rebind_event_thread();
            if let Err(error) = result {
                if error
                    .downcast_ref::<crate::ui::terminal::TerminalLifecycleError>()
                    .is_some()
                {
                    return Err(error);
                }
                self.renderer
                    .write_line(&format!("editor failed: {error:#}"), C_ERROR)?;
            }
            return Ok(());
        }

        if is_ctrl_h(key) {
            self.run_lazygit().await?;
            return Ok(());
        }

        // Chain prompt active: intercept Y/N/B keystrokes
        if self.renderer.chain_prompt.is_some() && !self.renderer.chain_but_mode {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.renderer.chain_prompt = None;
                    if let Some(phase) = self.chain.pending.take() {
                        self.chain.label_msg = None;
                        self.run_chain_transition(phase, None).await?;
                    }
                    return Ok(());
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.renderer.chain_prompt = None;
                    self.chain.pending = None;
                    self.chain.label_msg = None;
                    self.renderer
                        .write_line("chain declined — won't ask again this session", C_AGENT)?;
                    if let Some(ref name) = self.ui.context.current_prompt_name
                        && !self.ui.context.chain_declined.contains(name)
                    {
                        self.ui.context.chain_declined.push(name.clone());
                    }
                    return Ok(());
                }
                KeyCode::Char('b') | KeyCode::Char('B') => {
                    self.renderer.chain_but_mode = true;
                    self.renderer.chain_prompt = None;
                    self.input.clear_buffer();
                    self.chain.label_msg = self.chain.pending.map(|p| p.chain_label().to_string());
                    return Ok(());
                }
                _ => {
                    return Ok(());
                }
            }
        }

        // Chain but mode: Esc cancels back to ask
        if self.renderer.chain_but_mode && key.code == KeyCode::Esc {
            self.renderer.chain_but_mode = false;
            if let Some(phase) = self.chain.pending {
                self.renderer.chain_prompt = Some(ChainPrompt {
                    question: compact_str::CompactString::from(phase.chain_label()),
                });
                self.chain.label_msg = Some(phase.chain_label().to_string());
            }
            self.input.clear_buffer();
            return Ok(());
        }

        if let Some(mut text) = self.input.handle_key(key) {
            #[cfg(feature = "loop")]
            if self.chain.loop_state.as_ref().is_some_and(|ls| ls.active) && !text.starts_with('/')
            {
                self.renderer
                    .write_line("loop active: /loop stop to cancel", C_ERROR)?;
                return Ok(());
            }
            if self.renderer.is_scrolling() {
                self.renderer.scroll_to_bottom()?;
            }

            // Chain-of-prompts: handle text submission after B (but) mode
            if !self.run.is_running
                && let Some(phase) = self.chain.pending.take()
            {
                self.chain.label_msg = None;
                self.renderer.chain_but_mode = false;
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    self.chain.pending = Some(phase);
                    self.chain.label_msg = Some(phase.chain_label().to_string());
                    self.renderer.chain_prompt = Some(ChainPrompt {
                        question: compact_str::CompactString::from(phase.chain_label()),
                    });
                    return Ok(());
                }
                self.run_chain_transition(phase, Some(&trimmed)).await?;
                return Ok(());
            }

            match classify_submission(self.run.is_running, &text) {
                super::SubmitAction::Run => {}
                super::SubmitAction::Ignore => {
                    return Ok(());
                }
                super::SubmitAction::RejectWhileRunning => {
                    self.renderer.write_line(
                        "agent is running — wait for it to finish or press Ctrl-C before running a command",
                        C_ERROR,
                    )?;
                    return Ok(());
                }
                super::SubmitAction::Queue => {
                    self.run.pending_inputs.push_back(text.to_string());
                    self.renderer
                        .write_line(&format!("queued: {}", sanitize_output(&text)), C_TOOL)?;
                    return Ok(());
                }
            }

            // Bypass-slot handlers: /queue and /btw
            {
                let t = text.trim_start();
                if t == "/queue" || t.starts_with("/queue ") {
                    let arg = t.strip_prefix("/queue").unwrap_or("").trim();
                    self.run_queue_command(arg)?;
                    return Ok(());
                }
            }
            {
                let t = text.trim_start();
                if t == "/btw" || t.starts_with("/btw ") {
                    self.run_btw(&text).await?;
                    return Ok(());
                }
            }

            if self.handle_dot_command(&mut text).await? {
                return Ok(());
            }

            if text.starts_with('/') {
                self.run_slash_command(&text).await?;
            } else if text.starts_with('!') {
                self.run_bang_command(&text).await?;
            } else {
                for line in text.lines() {
                    let safe_line = sanitize_output(line);
                    self.renderer
                        .write_line(&format!("> {}", safe_line), Color::Green)?;
                }
                self.renderer.write_line("", Color::White)?;
                self.start_main_run(&text).await?;
            }
        }

        Ok(())
    }

    async fn handle_agent_event(&mut self, event: AgentEvent) -> anyhow::Result<()> {
        let terminal_error = matches!(&event, AgentEvent::Error { .. });
        let compaction_boundary_interactions = match &event {
            AgentEvent::CompactionBoundary { interactions } => Some(interactions.clone()),
            _ => None,
        };
        let failed_turn_has_progress =
            terminal_error && pending_main_turn_has_progress(&self.run, self.ui.session);
        match &event {
            AgentEvent::ToolCall { name, args, .. }
                if self.run.turn_trace.len() < TURN_TRACE_MAX =>
            {
                self.run
                    .turn_trace
                    .push(compact_str::CompactString::from(format!(
                        "→ {}",
                        crate::ui::utils::format_tool_call_summary(name, args)
                    )));
            }
            AgentEvent::ToolResult { output, .. } if self.run.turn_trace.len() < TURN_TRACE_MAX => {
                self.run
                    .turn_trace
                    .push(compact_str::CompactString::from(format!(
                        "← {}",
                        crate::extras::truncate::truncate_cjk(output, 500, "…")
                    )));
            }
            AgentEvent::Verification { passed, .. }
                if self.run.turn_trace.len() < TURN_TRACE_MAX =>
            {
                self.run
                    .turn_trace
                    .push(compact_str::CompactString::from(if *passed {
                        "✓ verification passed"
                    } else {
                        "✗ verification failed"
                    }));
            }
            AgentEvent::Done { .. } => {
                self.run.turn_trace.clear();
                self.run.awaiting_compaction_relief = false;
                self.run.pending_compaction_pressure = None;
            }
            AgentEvent::Error { .. } => {
                self.run.awaiting_compaction_relief = false;
                self.run.pending_compaction_pressure = None;
            }
            _ => {}
        }

        #[cfg(feature = "loop")]
        let loop_running = self.chain.loop_state.as_ref().is_some_and(|ls| ls.active);
        #[cfg(not(feature = "loop"))]
        let loop_running = false;

        let context_complete_usage_delta = matches!(
            &event,
            AgentEvent::UsageDelta {
                context_complete: true,
                ..
            }
        );
        let awaits_compaction_decision = matches!(
            &event,
            AgentEvent::UsageDelta {
                awaits_compaction_decision: true,
                ..
            }
        );
        let mid_turn_observation = if let AgentEvent::UsageDelta {
            usage,
            context_complete: true,
            awaits_compaction_decision: true,
        } = &event
            && self.run.is_running
            && !loop_running
            && !self.ui.cli.no_session
            && self.ui.cfg.resolve_compact_enabled()
            && self.ui.session.context_window > 0
            && let Some(threshold) = self.ui.cfg.resolve_mid_turn_compact_threshold()
        {
            let real_input_tokens = crate::session::Session::real_input_tokens(
                self.ui.cfg.is_anthropic_native(&self.ui.session.provider),
                usage.input_tokens,
                usage.total_tokens,
                usage.output_tokens,
                usage.cached_input_tokens,
                usage.cache_creation_input_tokens,
            );
            let pressure = real_input_tokens as f64 / self.ui.session.context_window as f64;
            Some((real_input_tokens, threshold, pressure))
        } else {
            None
        };
        let mid_turn_pressure =
            mid_turn_observation.filter(|(_, threshold, pressure)| pressure > threshold);

        // Preserve an interrupted turn once the model or a tool made observable
        // progress. Only a zero-progress failure restores the prompt and the
        // pre-turn snapshot.
        let terminal_success = matches!(&event, AgentEvent::Done { .. });
        let failed_prompt = (terminal_error && !failed_turn_has_progress)
            .then(|| rollback_pending_main_turn(&mut self.run, self.ui.session))
            .flatten();
        let handled = event_handler::handle_agent_event(
            event,
            &mut self.renderer,
            &mut self.run,
            &mut self.ui,
            &self.slash,
            &mut self.chain,
            #[cfg(feature = "loop")]
            &self.user_tx,
        )
        .await;

        if let Err(error) = handled {
            if let Some(text) = failed_prompt {
                self.input.load_text(&text);
            }
            if terminal_success {
                // The completed response/accounting is installed before
                // fallible presentation. Make that valid success durable even
                // when rendering or terminal post-processing fails.
                #[cfg(feature = "loop")]
                self.run.cancel_validation();
                if let Some(handle) = self.run.main_abort.take() {
                    handle.abort();
                }
                self.ui.sandbox.kill_active();
                self.run.is_running = false;
                if let Err(error) = self.run.retire(Duration::from_secs(5)).await {
                    tracing::warn!(%error, "TUI completed-run cleanup failed");
                }
                self.run.compaction_decision_tx = None;
                self.run.agent_line_started = false;
                self.run.response_buf.clear();
                self.run.response_start_block = None;
                if let Some(signals) = self.ui.status_signals.as_ref() {
                    signals.send_stop();
                }
                self.settle_success_transaction();
            } else if self.run.pending_turn.is_some() {
                // Presentation is part of the event handler and can fail after
                // a tool/stream event mutated live state. App unwind ends the
                // turn, so apply the same rollback policy as cancellation.
                self.fail_pending_main_turn().await;
            }
            return Err(error);
        }

        if terminal_error {
            self.run.turn_trace.clear();
        }

        if let Some(text) = failed_prompt {
            self.input.load_text(&text);
        }

        if let Some(interactions) = compaction_boundary_interactions {
            let pressure = self.run.pending_compaction_pressure.take().ok_or_else(|| {
                anyhow::anyhow!("runner reached an unrequested compaction boundary")
            })?;
            if let Err(error) = self.mid_turn_compact(pressure, &interactions).await {
                self.fail_pending_main_turn().await;
                return Err(error);
            }
            self.run.awaiting_compaction_relief = true;
            self.refresh()?;
            return Ok(());
        }

        let pressure_action = mid_turn_pressure_action(
            self.run.awaiting_compaction_relief,
            context_complete_usage_delta && mid_turn_observation.is_some(),
            mid_turn_pressure.is_some(),
        );
        // On irreducible pressure, leave the runner parked on its decision
        // receive and abort it below; acknowledging Continue would race the
        // hard stop against the next tool batch.
        if awaits_compaction_decision
            && pressure_action != MidTurnPressureAction::StopContextExhausted
            && let Some(decision_tx) = self.run.compaction_decision_tx.as_ref()
        {
            let decision = if pressure_action == MidTurnPressureAction::Compact {
                crate::agent::runner::CompactionBoundaryDecision::Compact
            } else {
                crate::agent::runner::CompactionBoundaryDecision::Continue
            };
            if decision_tx.send(decision).await.is_err() && self.run.is_running {
                self.fail_pending_main_turn().await;
                return Err(anyhow::anyhow!("agent compaction boundary channel closed"));
            }
        }

        match pressure_action {
            MidTurnPressureAction::StopContextExhausted => {
                let (real_input_tokens, threshold, _) =
                    mid_turn_pressure.expect("over-threshold action requires measured pressure");
                if let Err(error) = self.stop_context_exhausted(real_input_tokens, threshold) {
                    self.fail_pending_main_turn().await;
                    return Err(error);
                }
                self.run.awaiting_compaction_relief = false;
            }
            MidTurnPressureAction::Compact => {
                let (_, _, pressure) =
                    mid_turn_pressure.expect("over-threshold action requires measured pressure");
                self.run.pending_compaction_pressure = Some(pressure);
                self.renderer.write_line(
                    "context relief requested; waiting for the current tool batch to finish...",
                    Color::DarkGrey,
                )?;
            }
            MidTurnPressureAction::ClearReliefLatch => {
                self.run.awaiting_compaction_relief = false;
            }
            MidTurnPressureAction::Ignore => {}
        }
        if mid_turn_pressure.is_some() {
            if let Err(error) = self.refresh() {
                self.fail_pending_main_turn().await;
                return Err(error.into());
            }
            return Ok(());
        }

        self.finalize_turn().await?;
        Ok(())
    }

    #[cfg(feature = "loop")]
    async fn handle_loop_validation_event(
        &mut self,
        event: crate::event::LoopValidationEvent,
    ) -> anyhow::Result<()> {
        let current = event_handler::handle_loop_validation_event(
            event,
            &mut self.renderer,
            &mut self.run,
            &mut self.ui,
            &mut self.chain,
        )
        .await?;
        if current {
            self.finalize_turn().await?;
        }
        Ok(())
    }

    async fn finalize_turn(&mut self) -> anyhow::Result<()> {
        if !self.run.is_running {
            self.settle_success_transaction();
        }

        if !self.run.is_running
            && let Some(selection) = self.chain.dot_prompt_restore.take()
        {
            self.ui.context.restore_selection(selection);
            if let Some(perm) = &self.ui.permission {
                let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
                guard.restore_user_mode();
            }
            self.run.agent = None;
        }

        if !self.run.is_running
            && self.chain.pending.is_none()
            && let Some(ref name) = self.ui.context.current_prompt_name
            && !self.ui.context.chain_declined.contains(name)
            && let Some(phase) = crate::extras::chain::ChainPhase::from_prompt_name(name)
            && let Some(ref chain_cfg) = self.ui.cfg.chain
            && phase.is_enabled(chain_cfg)
        {
            self.chain.pending = Some(phase);
            self.chain.label_msg = Some(phase.chain_label().to_string());
            self.renderer.chain_but_mode = false;
            self.renderer.chain_prompt = Some(ChainPrompt {
                question: compact_str::CompactString::from(phase.chain_label()),
            });
        }

        if !self.run.is_running {
            self.run.main_abort = None;
            if let Some(next) = self.run.pending_inputs.pop_front() {
                self.renderer.chain_prompt = None;
                self.renderer.chain_but_mode = false;
                self.chain.pending = None;
                self.chain.label_msg = None;
                for line in next.lines() {
                    self.renderer
                        .write_line(&format!("> {}", sanitize_output(line)), Color::Green)?;
                }
                self.renderer.write_line("", Color::White)?;
                self.start_main_run(&next).await?;
            }
        }

        Ok(())
    }

    fn settle_success_transaction(&mut self) {
        if self.run.pending_turn.is_some() {
            // A `Done` event is terminal only once no validation, loop
            // iteration, or other continuation remains active. Commit the
            // complete transaction at that boundary, then discard rollback
            // state even when persistence warned and retained the prior file.
            let persisted = self.save_session_with_status();
            let pending = self
                .run
                .pending_turn
                .take()
                .expect("pending turn checked above");
            if persisted {
                for error in pending.commit_side_effects(!self.ui.cli.no_session) {
                    let _ = self.renderer.write_line(
                        &format!("warning: failed to append chat history entry: {error}"),
                        C_ERROR,
                    );
                }
            }
        }
    }

    /// Exception-safe failure transition with no required presentation. Used
    /// when rendering or mid-turn post-processing itself is what failed.
    async fn fail_pending_main_turn(&mut self) {
        let preserve_progress = preserve_pending_main_turn_progress(&mut self.run, self.ui.session);
        #[cfg(feature = "loop")]
        self.run.cancel_validation();
        if let Some(handle) = self.run.main_abort.take() {
            handle.abort();
        }
        self.ui.sandbox.kill_active();
        self.run.is_running = false;
        if let Err(error) = self.run.retire(Duration::from_secs(5)).await {
            tracing::warn!(%error, "TUI failed-run cleanup failed");
        }
        self.run.compaction_decision_tx = None;
        self.run.turn_trace.clear();
        self.run.awaiting_compaction_relief = false;
        self.run.pending_compaction_pressure = None;
        self.run.pending_inputs.clear();
        self.run.agent_line_started = false;
        self.run.response_buf.clear();
        self.run.response_start_block = None;
        if let Some(signals) = self.ui.status_signals.as_ref() {
            signals.send_stop();
        }
        if preserve_progress {
            self.settle_success_transaction();
        } else {
            if let Some(text) = rollback_pending_main_turn(&mut self.run, self.ui.session) {
                self.input.load_text(&text);
            }
            let _ = self.save_session();
        }
    }

    async fn abort_main_run(&mut self) -> anyhow::Result<()> {
        let preserve_progress = preserve_pending_main_turn_progress(&mut self.run, self.ui.session);
        #[cfg(feature = "loop")]
        let validation_active = self.run.cancel_validation();
        #[cfg(not(feature = "loop"))]
        let validation_active = false;

        if !validation_active {
            if let Some(handle) = self.run.main_abort.take() {
                handle.abort();
            }
            self.ui.sandbox.kill_active();
        }
        self.run.is_running = false;
        if let Some(ss) = self.ui.status_signals.as_ref() {
            ss.send_stop();
        }
        if let Err(error) = self.run.retire(Duration::from_secs(5)).await {
            tracing::warn!(%error, "TUI interrupted-run cleanup failed");
        }
        self.run.compaction_decision_tx = None;
        self.run.turn_trace.clear();
        self.run.awaiting_compaction_relief = false;
        self.run.pending_compaction_pressure = None;
        self.run.pending_inputs.clear();
        let failed_prompt = (!preserve_progress)
            .then(|| rollback_pending_main_turn(&mut self.run, self.ui.session))
            .flatten();
        #[cfg(feature = "loop")]
        if let Some(ref mut ls) = self.chain.loop_state {
            ls.active = false;
            self.chain.loop_label = None;
        }
        if !self.input.buffer.is_empty() {
            self.input.clear_buffer();
        }
        if let Some(text) = failed_prompt {
            self.input.load_text(&text);
        }
        if let Some(selection) = self.chain.dot_prompt_restore.take() {
            self.ui.context.restore_selection(selection);
            if let Some(perm) = &self.ui.permission {
                let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
                guard.restore_user_mode();
            }
            self.run.agent = None;
        }
        if preserve_progress {
            self.settle_success_transaction();
        } else {
            self.save_session()?;
        }
        self.renderer.write_line(
            "interrupted (changes may be partial; review with git diff)",
            C_ERROR,
        )?;
        Ok(())
    }

    async fn start_main_run(&mut self, text: &str) -> anyhow::Result<()> {
        // Preflight: if the pending payload is irreducibly too large, reject
        // locally before any provider I/O or session mutation.
        let text_tokens = crate::session::Session::estimate_tokens(text);
        // Rough conservative estimate: treat each media attachment as ~2 KiB of
        // token-equivalent overhead regardless of content type. Exact accounting
        // is provider-specific and not available at preflight time.
        #[cfg(feature = "multimodal")]
        const MEDIA_TOKENS_PER_ATTACHMENT: u64 = 2048;
        #[cfg(feature = "multimodal")]
        let pending_tokens = text_tokens.saturating_add(
            self.ui.session.pending_media.len() as u64 * MEDIA_TOKENS_PER_ATTACHMENT,
        );
        #[cfg(not(feature = "multimodal"))]
        let pending_tokens = text_tokens;
        let quick_models = config::quick_models_map(self.ui.cfg);
        let reserve = self.ui.cfg.resolve_reserve_tokens(
            &self.ui.session.model,
            &quick_models,
            self.ui.session.context_window,
        );
        if self
            .ui
            .session
            .is_irreducible_with_pending(reserve, pending_tokens)
        {
            let available = self
                .ui
                .session
                .context_window
                .saturating_sub(reserve)
                .saturating_sub(self.ui.session.overhead_tokens);
            let _ = self.renderer.write_line(
                &format!(
                    "message too large to fit: estimated {pending_tokens} tokens, \
                     only ~{available} tokens available after overhead and reserve \
                     (use /clear to free space or reduce message size)"
                ),
                C_ERROR,
            );
            return Ok(());
        }
        start_main_run(
            text,
            true,
            &mut self.run,
            &mut self.ui,
            &self.slash,
            &mut self.prebuild,
        )
        .await
    }

    async fn start_internal_main_run(&mut self, text: &str) -> anyhow::Result<()> {
        start_main_run(
            text,
            false,
            &mut self.run,
            &mut self.ui,
            &self.slash,
            &mut self.prebuild,
        )
        .await
    }

    async fn ensure_agent(&mut self) {
        super::resolve_prebuild(
            &mut self.run.agent,
            &mut self.ui,
            &mut self.prebuild,
            self.slash.reasoning_enabled,
        )
        .await;
        event_handler::ensure_agent(
            &mut self.run.agent,
            &mut self.ui,
            self.slash.reasoning_enabled,
        )
        .await;
    }

    async fn run_chain_transition(
        &mut self,
        phase: crate::extras::chain::ChainPhase,
        extra: Option<&str>,
    ) -> anyhow::Result<()> {
        let next_name = phase.next_prompt_name();
        self.ui.invalidate_prebuild();
        apply_prompt_mode(next_name, self.ui.context, &self.ui.permission);
        apply_prompt_model(
            next_name,
            &mut self.ui,
            &mut self.run.agent,
            self.slash.reasoning_enabled,
            &mut self.renderer,
        )
        .await;
        let base_msg = phase.transition_message().to_string();
        let msg = if let Some(extra) = extra {
            format!("{}\n\nAdditional instructions: {}", base_msg, extra)
        } else {
            base_msg
        };
        for line in msg.lines() {
            self.renderer
                .write_line(&format!("> {}", sanitize_output(line)), Color::Green)?;
        }
        self.renderer.write_line("", Color::White)?;
        self.run.agent = None;
        self.start_main_run(&msg).await?;
        Ok(())
    }

    async fn handle_dot_command(
        &mut self,
        text: &mut compact_str::CompactString,
    ) -> anyhow::Result<bool> {
        if !text.starts_with('.') {
            return Ok(false);
        }
        let after_dot = text[1..].trim_start();

        for line in text.lines() {
            let safe_line = sanitize_output(line);
            self.renderer
                .write_line(&format!("> {}", safe_line), Color::Green)?;
        }
        self.renderer.write_line("", Color::White)?;

        if after_dot.is_empty() {
            self.input.buffer = ".".into();
            self.input.cursor = 1;
            self.input.start_dot_picker();
            return Ok(true);
        }

        if let Some((prompt_name, msg)) = after_dot.split_once(char::is_whitespace) {
            let prompt_name = prompt_name.trim();
            let msg = msg.trim();
            if !prompt_name.is_empty() && self.ui.context.prompts.contains_key(prompt_name) {
                self.chain.dot_prompt_restore = Some(self.ui.context.active_selection());
                self.ui.invalidate_prebuild();
                apply_prompt_mode(prompt_name, self.ui.context, &self.ui.permission);
                apply_prompt_model(
                    prompt_name,
                    &mut self.ui,
                    &mut self.run.agent,
                    self.slash.reasoning_enabled,
                    &mut self.renderer,
                )
                .await;
                *text = msg.to_string().into();
                self.run.agent = None;
                return Ok(false);
            } else {
                self.renderer
                    .write_line(&format!("error: unknown prompt '{}'", prompt_name), C_ERROR)?;
                return Ok(true);
            }
        }

        let prompt_name = after_dot.trim();
        if self.ui.context.prompts.contains_key(prompt_name) {
            self.ui.invalidate_prebuild();
            apply_prompt_mode(prompt_name, self.ui.context, &self.ui.permission);
            apply_prompt_model(
                prompt_name,
                &mut self.ui,
                &mut self.run.agent,
                self.slash.reasoning_enabled,
                &mut self.renderer,
            )
            .await;
            self.run.agent = None;
            self.renderer
                .write_line(&format!("switched to prompt '{}'", prompt_name), C_AGENT)?;
            self.save_session()?;
            Ok(true)
        } else {
            self.renderer
                .write_line(&format!("error: unknown prompt '{}'", prompt_name), C_ERROR)?;
            Ok(true)
        }
    }

    fn run_queue_command(&mut self, arg: &str) -> anyhow::Result<()> {
        match arg {
            "clear" => {
                let n = self.run.pending_inputs.len();
                self.run.pending_inputs.clear();
                self.renderer
                    .write_line(&format!("queue cleared ({} removed)", n), C_TOOL)?;
            }
            "pop" => match self.run.pending_inputs.pop_back() {
                Some(x) => self
                    .renderer
                    .write_line(&format!("unqueued: {}", sanitize_output(&x)), C_TOOL)?,
                None => self.renderer.write_line("queue is empty", C_TOOL)?,
            },
            "" | "ls" | "list" => {
                if self.run.pending_inputs.is_empty() {
                    self.renderer.write_line("queue is empty", C_TOOL)?;
                } else {
                    self.renderer.write_line(
                        &format!("queued ({}):", self.run.pending_inputs.len()),
                        C_TOOL,
                    )?;
                    for (i, q) in self.run.pending_inputs.iter().enumerate() {
                        self.renderer
                            .write_line(&format!("  {}. {}", i + 1, sanitize_output(q)), C_TOOL)?;
                    }
                }
            }
            _ => self
                .renderer
                .write_line("usage: /queue [ls|clear|pop]", C_ERROR)?,
        }
        Ok(())
    }

    async fn run_btw(&mut self, text: &str) -> anyhow::Result<()> {
        for line in text.lines() {
            self.renderer
                .write_line(&format!("> {}", sanitize_output(line)), Color::Green)?;
        }
        self.renderer.write_line("", Color::White)?;
        let btw_text = text
            .trim_start()
            .strip_prefix("/btw")
            .map(|s| s.trim())
            .unwrap_or("");
        if btw_text.is_empty() {
            self.renderer.write_line("usage: /btw <message>", C_AGENT)?;
            return Ok(());
        }
        if self.btw_inflight >= BTW_MAX_INFLIGHT {
            self.renderer.write_line(
                &format!(
                    "[btw] too many side questions in flight (max {}); try again when one completes",
                    BTW_MAX_INFLIGHT
                ),
                C_ERROR,
            )?;
            return Ok(());
        }
        let id = self.btw_next_id;
        self.btw_next_id = self.btw_next_id.wrapping_add(1);
        let snapshot = crate::agent::runner::build_btw_snapshot_with_tool_result_retention(
            self.ui.session,
            &self.run.turn_trace,
            self.run.is_running,
            self.ui.cfg.resolve_keep_recent_tool_results(),
        );
        let model = self
            .ui
            .client
            .completion_model(self.ui.session.model.to_string());
        let temperature =
            crate::config::resolve_temperature(self.ui.cli, self.ui.cfg, &self.ui.session.model);
        let extra_body = crate::config::resolve_extra_body(self.ui.cfg, &self.ui.session.model);
        let btw_agent = crate::provider::build_btw_agent(
            model,
            self.ui.cli,
            self.ui.cfg,
            self.ui.context,
            &self.ui.workspace,
            &self.ui.permission,
            &self.ui.ask_tx,
            &self.ui.session.id,
            self.ui.session.tool_result_spills.clone(),
            self.slash.reasoning_enabled,
            temperature,
            extra_body,
        );
        let runner = btw_agent.spawn_btw(
            btw_text.to_string(),
            snapshot,
            self.btw_tx.clone(),
            id,
            self.ui.cfg.retry.clone(),
        );
        self.btw_abort
            .push((id, runner.abort_handle, runner.task, runner.work_scope));
        self.btw_inflight += 1;
        self.renderer
            .write_line(&format!("[btw #{}] thinking...", id), C_BTW)?;
        Ok(())
    }

    async fn run_slash_command(&mut self, text: &str) -> anyhow::Result<()> {
        for line in text.lines() {
            let safe_line = sanitize_output(line);
            self.renderer
                .write_line(&format!("> {}", safe_line), Color::Green)?;
        }
        self.renderer.write_line("", Color::White)?;

        // Commands that hand the tty to a synchronous stdin consumer must not
        // race the crossterm event thread for keystrokes (same pattern as
        // `run_lazygit`): stop it before, rebind it after.
        let needs_tty = slash_command_needs_tty(text);
        if needs_tty {
            self.pause_event_thread();
        }
        let result = handle_slash(
            text,
            &mut self.renderer,
            &mut self.input,
            &mut self.run,
            &mut self.ui,
            &mut self.slash,
            &mut self.chain,
            &mut self.terminal_guard,
        )
        .await;
        if needs_tty {
            self.rebind_event_thread();
        }

        if result.as_ref().is_err_and(|error| {
            error
                .downcast_ref::<crate::ui::terminal::TerminalLifecycleError>()
                .is_some()
        }) {
            return result;
        }

        {
            let provider = self.ui.session.provider.to_string();
            let is_custom = self.ui.cfg.custom_providers_map().contains_key(&provider);
            let ids = crate::ui::slash::warm_model_cache(
                &provider,
                is_custom,
                &self.ui.client,
                self.ui.cli,
                self.ui.cfg,
            )
            .await;
            self.input.set_live_model_names(ids);
        }

        self.handle_slash_result(result).await?;
        self.save_session()?;
        Ok(())
    }

    async fn handle_slash_result(&mut self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        match result {
            Err(e) if e.to_string().starts_with("DEFER_COMPRESS:") => {
                let err_msg = e.to_string();
                let instructions = err_msg.strip_prefix("DEFER_COMPRESS:").and_then(|s| {
                    let s = s.trim();
                    if s.is_empty() || s == "(none)" {
                        None
                    } else {
                        Some(s.to_string())
                    }
                });
                let compress_result = handle_compress(
                    instructions.as_deref(),
                    false,
                    &mut self.run,
                    &mut self.renderer,
                    &mut self.ui,
                    self.slash.reasoning_enabled,
                )
                .await;
                if let Err(e) = compress_result {
                    self.renderer
                        .write_line(&format!("compress error: {}", e), C_ERROR)?;
                }
                let _ = self.save_session();
            }
            #[cfg(feature = "mcp")]
            Err(e)
                if e.to_string()
                    .starts_with(crate::ui::slash::settings::DEFER_MCP_LOGIN) =>
            {
                let server = e
                    .to_string()
                    .strip_prefix(crate::ui::slash::settings::DEFER_MCP_LOGIN)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let resolved = self
                    .ui
                    .cfg
                    .mcp_servers
                    .as_ref()
                    .and_then(|m| m.get(&server))
                    .and_then(|s| {
                        if let crate::extras::mcp::config::McpServerConfig::Url {
                            url, oauth, ..
                        } = s
                        {
                            oauth
                                .as_ref()
                                .and_then(|o| o.settings())
                                .map(|set| (url.clone(), set))
                        } else {
                            None
                        }
                    });
                match resolved {
                    Some((url, settings)) => {
                        self.renderer.write_line(
                            &format!("starting OAuth login for '{}'...", server),
                            C_AGENT,
                        )?;
                        match crate::extras::mcp::oauth::begin_login(&server, &url, &settings).await
                        {
                            Ok(login) => {
                                let copy_status = match copy_to_clipboard(&login.auth_url).await {
                                    Ok(ClipboardCopyOutcome::Confirmed) => {
                                        "open this URL to authorize (copied to clipboard):"
                                    }
                                    Ok(ClipboardCopyOutcome::FallbackRequested) => {
                                        "open this URL to authorize (terminal copy requested):"
                                    }
                                    Err(_) => {
                                        "open this URL to authorize (could not copy to clipboard):"
                                    }
                                };
                                self.renderer.write_line(copy_status, C_AGENT)?;
                                self.renderer.write_line(&login.auth_url, Color::Cyan)?;
                                self.renderer.write_line(
                                    &format!(
                                        "waiting for authorization on 127.0.0.1:{} in the background...",
                                        settings.redirect_port()
                                    ),
                                    Color::DarkGrey,
                                )?;
                                let tx = self.user_tx.clone();
                                let sname = compact_str::CompactString::new(&server);
                                tokio::spawn(async move {
                                    let error = login
                                        .wait_for_callback(Duration::from_secs(180))
                                        .await
                                        .err()
                                        .map(|e| compact_str::CompactString::new(e.to_string()));
                                    let _ = tx
                                        .send(UserEvent::McpLoginDone {
                                            server: sname,
                                            error,
                                        })
                                        .await;
                                });
                            }
                            Err(err) => {
                                self.renderer.write_line(
                                    &format!("login setup failed for '{}': {}", server, err),
                                    C_ERROR,
                                )?;
                            }
                        }
                    }
                    None => {
                        self.renderer.write_line(
                            &format!(
                                "cannot start login for '{}' (not an OAuth URL server)",
                                server
                            ),
                            C_ERROR,
                        )?;
                    }
                }
            }
            #[cfg(feature = "git-worktree")]
            Err(e)
                if e.downcast_ref::<crate::extras::git_worktree::DeferredWorktreeAction>()
                    .is_some() =>
            {
                let action = e
                    .downcast_ref::<crate::extras::git_worktree::DeferredWorktreeAction>()
                    .unwrap();
                match action {
                    crate::extras::git_worktree::DeferredWorktreeAction::Switch {
                        path,
                        branch,
                    } => {
                        super::rebind_worktree_workspace(
                            self.ui.session,
                            self.ui.context,
                            &self.ui.permission,
                            &mut self.ui.workspace,
                            &mut self.ui.sandbox,
                            path,
                            self.ui.cli.resolve_no_context_files(self.ui.cfg),
                        )?;
                        self.refresh_worktree_workspace_context().await?;
                        self.renderer.write_line(
                            &format!(
                                "worktree created: branch '{}' at {}",
                                branch,
                                path.display()
                            ),
                            C_AGENT,
                        )?;
                    }
                    crate::extras::git_worktree::DeferredWorktreeAction::Merge { info, target } => {
                        self.handle_worktree_merge(info.clone(), target.clone(), true)
                            .await?;
                    }
                    crate::extras::git_worktree::DeferredWorktreeAction::Exit { main_path } => {
                        super::rebind_worktree_workspace(
                            self.ui.session,
                            self.ui.context,
                            &self.ui.permission,
                            &mut self.ui.workspace,
                            &mut self.ui.sandbox,
                            main_path,
                            self.ui.cli.resolve_no_context_files(self.ui.cfg),
                        )?;
                        apply_current_prompt_mode(self.ui.context, &self.ui.permission);
                        #[cfg(feature = "mcp")]
                        rebind_mcp_manager(
                            &mut self.ui.mcp_manager,
                            self.ui.cfg,
                            &self.ui.workspace,
                        )
                        .await;
                        let new_agent = self
                            .ui
                            .agent_build_ctx()
                            .rebuild_agent(&self.ui.session.model, self.slash.reasoning_enabled)
                            .await;
                        self.run.agent = Some(new_agent);
                        render_session(
                            &mut self.renderer,
                            self.ui.session,
                            self.ui.cli,
                            self.ui.cfg,
                            self.ui.context,
                        )?;
                        self.renderer.write_line(
                            &format!("returned to main repo at {}", main_path.display()),
                            C_AGENT,
                        )?;
                    }
                }
            }
            Err(e) if e.to_string().starts_with("DEFER_INIT:") => {
                let prompt = e
                    .to_string()
                    .strip_prefix("DEFER_INIT:")
                    .unwrap_or("")
                    .to_string();
                self.ensure_agent().await;
                self.run.request_tool_results_cleared =
                    self.ui.session.tool_results_cleared_for_retention(
                        self.ui.cfg.resolve_keep_recent_tool_results(),
                    );
                let history =
                    crate::agent::runner::convert_history_shared_with_tool_result_retention(
                        self.ui.session,
                        self.ui.cfg.resolve_keep_recent_tool_results(),
                    );
                let runner = self
                    .run
                    .agent
                    .as_ref()
                    .unwrap()
                    .clone()
                    .spawn_runner(
                        prompt,
                        history,
                        self.ui.cfg.retry.clone(),
                        #[cfg(feature = "hooks")]
                        None,
                    )
                    .await;
                self.run.compaction_decision_tx = runner.compaction_decision_tx;
                self.run.agent_rx = Some(runner.event_rx);
                self.run.main_abort = Some(runner.abort_handle);
                self.run.is_running = true;
                if let Some(ss) = self.ui.status_signals.as_ref() {
                    ss.send_start();
                }
            }
            Err(e) if e.to_string().starts_with("DEFER_REVIEW:") => {
                let msg = e
                    .to_string()
                    .strip_prefix("DEFER_REVIEW:")
                    .unwrap_or("")
                    .to_string();
                self.chain.dot_prompt_restore = self.ui.context.one_shot_restore.take();
                self.start_internal_main_run(&msg).await?;
            }
            #[cfg(feature = "memory")]
            Err(e) if e.to_string().starts_with("DEFER_EDITOR:") => {
                let path = e
                    .to_string()
                    .strip_prefix("DEFER_EDITOR:")
                    .unwrap_or("")
                    .to_string();
                let editor = self
                    .ui
                    .cfg
                    .editor
                    .clone()
                    .or_else(|| std::env::var("EDITOR").ok())
                    .unwrap_or_else(|| "editor".to_string());
                // The editor owns the tty: stop the event thread first so it
                // cannot steal keystrokes, rebind it once the terminal is back.
                self.pause_event_thread();
                self.terminal_guard.suspend()?;
                let edit_result =
                    crate::ui::slash::edit_memory_file(std::path::Path::new(&path), &editor);
                let resume_result = self.terminal_guard.resume();
                self.rebind_event_thread();
                resume_result?;
                render_session(
                    &mut self.renderer,
                    self.ui.session,
                    self.ui.cli,
                    self.ui.cfg,
                    self.ui.context,
                )?;
                match edit_result {
                    Ok(true) => self
                        .renderer
                        .write_line(&format!("updated memory {}", path), C_AGENT)?,
                    Ok(false) => self
                        .renderer
                        .write_line(&format!("memory unchanged {}", path), C_AGENT)?,
                    Err(error) => self.renderer.write_line(
                        &format!("memory editor failed for {}: {}", path, error),
                        C_ERROR,
                    )?,
                }
            }
            Err(e) if crate::ui::slash::is_persistence_restart_required(&e) => {
                return Err(e);
            }
            Err(e)
                if e.downcast_ref::<crate::ui::terminal::TerminalLifecycleError>()
                    .is_some() =>
            {
                return Err(e);
            }
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::Interrupted) =>
            {
                return Err(e);
            }
            Err(e) => {
                self.renderer
                    .write_line(&format!("error: {}", e), C_ERROR)?;
            }
            Ok(()) => {
                self.save_session()?;
                #[cfg(feature = "loop")]
                if self
                    .chain
                    .loop_state
                    .as_ref()
                    .is_some_and(|ls| ls.active && ls.iteration == 0 && !self.run.is_running)
                {
                    #[allow(unused_variables)]
                    let (prompt, label, active) = {
                        let ls = self.chain.loop_state.as_mut().unwrap();
                        ls.iteration = 1;
                        (ls.build_prompt(), ls.iteration_label(), ls.active)
                    };
                    self.ensure_agent().await;
                    self.run.request_tool_results_cleared = 0;
                    let runner = self
                        .run
                        .agent
                        .as_ref()
                        .unwrap()
                        .clone()
                        .spawn_runner(
                            prompt,
                            Vec::new(),
                            self.ui.cfg.retry.clone(),
                            #[cfg(feature = "hooks")]
                            Some(crate::extras::hooks::LoopInfo {
                                iteration: 1,
                                active,
                            }),
                        )
                        .await;
                    self.run.compaction_decision_tx = runner.compaction_decision_tx;
                    self.run.agent_rx = Some(runner.event_rx);
                    self.run.main_abort = Some(runner.abort_handle);
                    self.run.is_running = true;
                    self.chain.loop_label = Some(label);
                }
            }
        }
        Ok(())
    }

    async fn run_bang_command(&mut self, text: &str) -> anyhow::Result<()> {
        let command_is_empty = text
            .strip_prefix('!')
            .is_none_or(|command| command.trim().is_empty());
        if command_is_empty {
            self.renderer
                .write_line("error: empty command after '!'", C_ERROR)?;
            return Ok(());
        }
        for line in text.lines() {
            let safe_line = sanitize_output(line);
            self.renderer
                .write_line(&format!("> {}", safe_line), Color::Green)?;
        }
        self.renderer.write_line("", Color::White)?;

        let cancellation = CommandCancellation::new();
        let sandbox = self.ui.sandbox.clone();
        let operation =
            sandbox.run_explicit_shell(text, DEFAULT_COMMAND_LIMITS, Some(&cancellation));
        tokio::pin!(operation);
        let run = loop {
            tokio::select! {
                result = &mut operation => break result?,
                event = self.user_rx.recv() => {
                    let Some(event) = event else {
                        cancellation.cancel();
                        break operation.await?;
                    };
                    let interrupt = matches!(
                        &event,
                        UserEvent::Key(key)
                            if matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                    );
                    if interrupt {
                        cancellation.cancel();
                    } else {
                        self.deferred_user_events.push_back(event);
                    }
                }
            }
        };
        let result = run.rendered_output();

        for line in result.lines() {
            let safe_line = sanitize_output(line);
            self.renderer
                .write_line(&safe_line, if run.succeeded() { C_AGENT } else { C_ERROR })?;
        }
        self.renderer.write_line("", Color::White)?;

        self.ui.session.add_message(MessageRole::User, text);
        self.ui.session.add_message(MessageRole::Assistant, &result);
        if !self.ui.cli.no_session {
            let _ = crate::session::chat_history::append_entry(
                &crate::session::chat_history::ChatHistoryEntry {
                    content: text.to_string(),
                    timestamp: self.ui.session.updated_at.clone(),
                },
            );
        }
        Ok(())
    }

    async fn mid_turn_compact(
        &mut self,
        pressure: f64,
        interactions: &[Message],
    ) -> anyhow::Result<()> {
        mid_turn_compact_and_respawn(
            pressure,
            interactions,
            &mut self.renderer,
            &mut self.run,
            &mut self.ui,
            &self.slash,
        )
        .await
    }

    fn stop_context_exhausted(&mut self, prompt_tokens: u64, threshold: f64) -> anyhow::Result<()> {
        let preserve_progress = preserve_pending_main_turn_progress(&mut self.run, self.ui.session);
        let rendered = stop_turn_context_exhausted(
            prompt_tokens,
            threshold,
            &mut self.renderer,
            &self.ui,
            &mut self.run,
        );
        if preserve_progress {
            self.settle_success_transaction();
        } else {
            if let Some(text) = rollback_pending_main_turn(&mut self.run, self.ui.session) {
                self.input.load_text(&text);
            }
            self.save_session()?;
        }
        rendered
    }

    fn handle_btw_event(&mut self, bev: BtwEvent) -> anyhow::Result<()> {
        match bev {
            BtwEvent::Done {
                id,
                response,
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cache_creation_input_tokens,
            } => {
                self.btw_total_cost += crate::pricing::estimate_cost(
                    crate::pricing::billable_input_tokens(
                        self.ui.cfg.is_anthropic_native(&self.ui.session.provider),
                        input_tokens,
                        cached_input_tokens,
                        cache_creation_input_tokens,
                    ),
                    output_tokens,
                    self.ui.session.input_token_cost,
                    self.ui.session.output_token_cost,
                );
                self.btw_total_in = self.btw_total_in.saturating_add(input_tokens);
                self.btw_total_out = self.btw_total_out.saturating_add(output_tokens);
                self.btw_abort.retain(|(i, _, _, _)| *i != id);
                self.btw_inflight = self.btw_inflight.saturating_sub(1);
                self.renderer
                    .write_line(&format!("[btw #{}] answer:", id), C_BTW)?;
                for line in response.lines() {
                    self.renderer.write_line(&sanitize_output(line), C_AGENT)?;
                }
                self.renderer.write_line("", Color::White)?;
            }
            BtwEvent::Error { id, message } => {
                self.btw_abort.retain(|(i, _, _, _)| *i != id);
                self.btw_inflight = self.btw_inflight.saturating_sub(1);
                self.renderer.write_line(
                    &format!("[btw #{}] error: {}", id, sanitize_output(&message)),
                    C_ERROR,
                )?;
            }
        }
        Ok(())
    }

    async fn take_prebuild(&mut self, prebuilt: PrebuildPayload) {
        let current = self
            .prebuild
            .as_ref()
            .is_some_and(|prebuild| prebuild.agent_is_current());
        super::adopt_prebuild(
            &mut self.run.agent,
            &mut self.ui,
            prebuilt,
            current,
            self.slash.reasoning_enabled,
        )
        .await;
    }

    #[cfg(feature = "mcp")]
    async fn handle_mcp_login_done(
        &mut self,
        server: compact_str::CompactString,
        error: Option<compact_str::CompactString>,
    ) -> anyhow::Result<()> {
        if let Some(err) = error {
            self.renderer
                .write_line(&format!("login failed for '{}': {}", server, err), C_ERROR)?;
        } else {
            let server = server.to_string();
            let server_cfg = self
                .ui
                .cfg
                .mcp_servers
                .as_ref()
                .and_then(|m| m.get(&server).cloned());
            match (self.ui.mcp_manager.as_mut(), server_cfg) {
                (Some(mgr), Some(scfg)) => match mgr
                    .reconnect_in_binding(&server, &scfg, &self.ui.workspace)
                    .await
                {
                    Ok(()) => {
                        let new_agent = self
                            .ui
                            .agent_build_ctx()
                            .rebuild_agent(&self.ui.session.model, self.slash.reasoning_enabled)
                            .await;
                        self.run.agent = Some(new_agent);
                        self.renderer.write_line(
                            &format!("authorized and connected '{}'", server),
                            C_AGENT,
                        )?;
                    }
                    Err(err) => {
                        self.renderer.write_line(
                            &format!("authorized '{}' but reconnect failed: {}", server, err),
                            C_ERROR,
                        )?;
                    }
                },
                _ => {
                    self.renderer.write_line(
                        &format!("authorized '{}' (will connect on next start)", server),
                        C_AGENT,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Stop and join the crossterm event thread so a synchronous stdin
    /// consumer (editor, pager, y/N prompt) is the only tty reader. Pair with
    /// `rebind_event_thread` once the terminal is resumed.
    fn pause_event_thread(&mut self) {
        pause_input_reader(
            &self.running,
            &mut self.event_handle,
            &mut self.user_rx,
            &mut self.deferred_user_events,
        );
    }

    fn rebind_event_thread(&mut self) {
        self.pause_event_thread();
        // Terminal resumption clears the screen even when the draft is unchanged.
        self.renderer.invalidate();
        self.running = Arc::new(AtomicBool::new(true));
        // Background producers retain clones of this sender across handoffs.
        self.event_handle = Some(spawn_event_thread(
            self.user_tx.clone(),
            self.running.clone(),
        ));
    }

    async fn run_lazygit(&mut self) -> anyhow::Result<()> {
        const PROBE_LIMITS: CommandLimits = CommandLimits {
            timeout: Duration::from_secs(5),
            stdout_bytes: 64 * 1024,
            stderr_bytes: 64 * 1024,
            combined_bytes: 64 * 1024,
        };
        const INTERACTIVE_LIMITS: SupportCommandLimits = SupportCommandLimits {
            timeout: Duration::from_secs(24 * 60 * 60),
        };
        let cwd = self.ui.workspace.root().to_path_buf();

        let mut probe = tokio::process::Command::new("lazygit");
        probe.arg("--version");
        let probe_result = self
            .ui
            .sandbox
            .output_support_command(probe, PROBE_LIMITS)
            .await;
        let available = probe_result.as_ref().is_ok_and(|output| {
            output.status == CommandStatus::Completed
                && output
                    .exit_status
                    .as_ref()
                    .is_some_and(|status| status.success())
        });
        match &probe_result {
            Ok(_) if available => {
                tracing::info!(
                    target: "zerostack::audit::support_utility",
                    trust_class = "TC-SUPPORT-UTILITY",
                    utility = "lazygit-version-probe",
                    cwd = %cwd.display(),
                    boundary = "user-trusted-bypass",
                    outcome = "success",
                    "support utility probe completed"
                );
            }
            Ok(output) => {
                tracing::warn!(
                    target: "zerostack::audit::support_utility",
                    trust_class = "TC-SUPPORT-UTILITY",
                    utility = "lazygit-version-probe",
                    cwd = %cwd.display(),
                    boundary = "user-trusted-bypass",
                    outcome = ?output.status,
                    "support utility probe did not complete successfully"
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "zerostack::audit::support_utility",
                    trust_class = "TC-SUPPORT-UTILITY",
                    utility = "lazygit-version-probe",
                    cwd = %cwd.display(),
                    boundary = "user-trusted-bypass",
                    outcome = "runner-error",
                    error = %error,
                    "support utility probe failed"
                );
            }
        }
        if !available {
            self.renderer.write_line(
                "warning: lazygit unavailable or version probe failed — install it (https://github.com/jesseduffield/lazygit)",
                C_ERROR,
            )?;
            return Ok(());
        }
        self.pause_event_thread();
        self.terminal_guard.suspend()?;
        let mut command = tokio::process::Command::new("lazygit");
        command.current_dir(self.ui.workspace.root());
        let result = self
            .ui
            .sandbox
            .clone()
            .status_support_command(
                command,
                INTERACTIVE_LIMITS,
                SupportCommandAudit::new("lazygit", "user-trusted-bypass"),
            )
            .await;
        let resume_result = self.terminal_guard.resume();
        self.rebind_event_thread();
        resume_result?;
        match result {
            Ok(output)
                if output.status == CommandStatus::Completed
                    && output.exit_status.is_some_and(|status| status.success()) => {}
            Ok(output) => {
                self.renderer.write_line(
                    &format!("warning: lazygit ended with {:?}", output.status),
                    C_ERROR,
                )?;
            }
            Err(error) => {
                tracing::warn!(
                    target: "zerostack::audit::support_utility",
                    trust_class = "TC-SUPPORT-UTILITY",
                    utility = "lazygit",
                    cwd = %cwd.display(),
                    boundary = "user-trusted-bypass",
                    outcome = "runner-error",
                    error = %error,
                    "support utility failed"
                );
                self.renderer
                    .write_line(&format!("warning: lazygit failed: {error}"), C_ERROR)?;
            }
        }
        Ok(())
    }

    fn save_session(&mut self) -> anyhow::Result<()> {
        if self.run.pending_turn.is_some() {
            return Ok(());
        }
        self.save_session_with_status();
        Ok(())
    }

    /// Save without letting a presentation failure interrupt a terminal state
    /// transition. `true` means the transaction has an authoritative backing
    /// snapshot (or persistence was explicitly disabled).
    fn save_session_with_status(&mut self) -> bool {
        if self.ui.cli.no_session {
            return true;
        }
        match crate::session::storage::save_session(self.ui.session) {
            Ok(()) => true,
            Err(error) => {
                let _ = self.renderer.write_line(
                    &format!("warning: failed to save session: {error}"),
                    C_ERROR,
                );
                false
            }
        }
    }

    #[cfg(feature = "git-worktree")]
    async fn handle_worktree_auto_merge(&mut self) -> anyhow::Result<()> {
        if !self.ui.cli.resolve_wt_auto_merge(self.ui.cfg) {
            return Ok(());
        }
        let info = match crate::extras::git_worktree::detect(self.ui.workspace.root()).await {
            Some(i) => i,
            None => return Ok(()),
        };
        let target = crate::extras::git_worktree::default_branch(&info.main_repo_path)
            .await
            .unwrap_or_else(|| "main".to_string());

        self.handle_worktree_merge(info, target, false).await
    }

    #[cfg(feature = "git-worktree")]
    async fn handle_worktree_merge(
        &mut self,
        info: crate::extras::git_worktree::WorktreeInfo,
        target: String,
        refresh_ui: bool,
    ) -> anyhow::Result<()> {
        let _ = self.renderer.write_line(
            &format!("merging worktree '{}' into '{}'...", info.branch, target),
            C_AGENT,
        );
        let mut proceed = true;
        let dirty = match crate::extras::git_worktree::worktree_has_uncommitted(&info.worktree_path)
            .await
        {
            Ok(dirty) => dirty,
            Err(error) => {
                let _ = self.renderer.write_line(
                    &format!("cannot verify worktree status; merge aborted: {error}"),
                    C_ERROR,
                );
                return Ok(());
            }
        };
        if dirty {
            let _ = self.renderer.write_line(
                "worktree has uncommitted changes. [c]ommit all and continue  [a]bort merge",
                C_PERM,
            );
            if let Some(ss) = self.ui.status_signals.as_ref() {
                ss.send_git_conflict();
            }
            let action =
                read_worktree_choice(&mut self.user_rx, &mut self.deferred_user_events, 'c').await;
            match action {
                WorktreePromptChoice::Proceed => {
                    if let Err(e) =
                        crate::extras::git_worktree::worktree_auto_commit_all(&info.worktree_path)
                            .await
                    {
                        let _ = self
                            .renderer
                            .write_line(&format!("auto-commit failed: {}", e), C_ERROR);
                        proceed = false;
                    } else {
                        match crate::extras::git_worktree::worktree_has_uncommitted(
                            &info.worktree_path,
                        )
                        .await
                        {
                            Ok(false) => {
                                let _ = self.renderer.write_line(
                                    "committed all worktree changes, proceeding with merge",
                                    C_AGENT,
                                );
                            }
                            Ok(true) => {
                                let _ = self.renderer.write_line(
                                    "worktree remained dirty after auto-commit; merge aborted",
                                    C_ERROR,
                                );
                                proceed = false;
                            }
                            Err(error) => {
                                let _ = self.renderer.write_line(
                                    &format!(
                                        "cannot verify worktree status after auto-commit; merge aborted: {error}"
                                    ),
                                    C_ERROR,
                                );
                                proceed = false;
                            }
                        }
                    }
                }
                WorktreePromptChoice::Abort => {
                    let _ = self
                        .renderer
                        .write_line("merge aborted, worktree left untouched", C_AGENT);
                    proceed = false;
                }
            }
        }
        if !proceed {
            return Ok(());
        }
        let (state, outcome) = crate::extras::git_worktree::try_merge(&info, &target).await;
        let mut state = state;
        match outcome {
            crate::extras::git_worktree::MergeOutcome::Success => {
                super::rebind_worktree_workspace(
                    self.ui.session,
                    self.ui.context,
                    &self.ui.permission,
                    &mut self.ui.workspace,
                    &mut self.ui.sandbox,
                    &info.main_repo_path,
                    self.ui.cli.resolve_no_context_files(self.ui.cfg),
                )?;
                let merge_result = retire_workspace_owners_and_complete_merge(
                    &mut self.run,
                    &mut self.btw_abort,
                    &mut self.btw_inflight,
                    &mut self.prebuild,
                    #[cfg(feature = "mcp")]
                    &mut self.ui.mcp_manager,
                    &mut state,
                    std::time::Duration::from_secs(5),
                )
                .await?;
                if refresh_ui {
                    self.refresh_worktree_workspace_context().await?;
                }
                match merge_result {
                    Ok(()) => {
                        let _ = self.renderer.write_line(
                            &format!("merged '{}' into '{}' and cleaned up", info.branch, target),
                            C_AGENT,
                        );
                    }
                    Err(e) => {
                        let _ = self.renderer.write_line(
                            &format!("merge succeeded but cleanup failed: {}", e),
                            C_ERROR,
                        );
                    }
                }
            }
            crate::extras::git_worktree::MergeOutcome::Conflicts(files) => {
                let _ = self.renderer.write_line(
                    &format!("merge conflict in {} file(s):", files.len()),
                    C_ERROR,
                );
                for f in &files {
                    let _ = self.renderer.write_line(&format!("  {}", f), C_ERROR);
                }
                if let Some(ss) = self.ui.status_signals.as_ref() {
                    ss.send_git_conflict();
                }
                let _ = self
                    .renderer
                    .write_line("[a]bort  [l]eave for manual resolution", C_PERM);

                let action =
                    read_worktree_choice(&mut self.user_rx, &mut self.deferred_user_events, 'l')
                        .await;

                match action {
                    WorktreePromptChoice::Abort => {
                        if let Err(error) =
                            crate::extras::git_worktree::cancel_merge(&mut state).await
                        {
                            let _ = self.renderer.write_line(
                                &format!("merge cancellation failed; cleanup skipped: {error}"),
                                C_ERROR,
                            );
                            return Ok(());
                        }
                        super::rebind_worktree_workspace(
                            self.ui.session,
                            self.ui.context,
                            &self.ui.permission,
                            &mut self.ui.workspace,
                            &mut self.ui.sandbox,
                            &info.main_repo_path,
                            self.ui.cli.resolve_no_context_files(self.ui.cfg),
                        )?;
                        if refresh_ui {
                            self.refresh_worktree_workspace_context().await?;
                        }
                        let _ = self.renderer.write_line(
                            "merge aborted, restored original state; worktree and branch retained",
                            C_AGENT,
                        );
                    }
                    WorktreePromptChoice::Proceed => {
                        super::rebind_worktree_workspace(
                            self.ui.session,
                            self.ui.context,
                            &self.ui.permission,
                            &mut self.ui.workspace,
                            &mut self.ui.sandbox,
                            &info.main_repo_path,
                            self.ui.cli.resolve_no_context_files(self.ui.cfg),
                        )?;
                        if refresh_ui {
                            self.refresh_worktree_workspace_context().await?;
                        }
                        let _ = self.renderer.write_line(
                            &format!(
                                "conflict state left in {} for manual resolution; source worktree, branch, and any pre-merge stash retained",
                                info.main_repo_path.display()
                            ),
                            C_AGENT,
                        );
                    }
                }
            }
            crate::extras::git_worktree::MergeOutcome::Error(e) => {
                let _ = self
                    .renderer
                    .write_line(&format!("merge failed: {}", e), C_ERROR);
            }
        }
        Ok(())
    }

    #[cfg(feature = "git-worktree")]
    async fn refresh_worktree_workspace_context(&mut self) -> anyhow::Result<()> {
        apply_current_prompt_mode(self.ui.context, &self.ui.permission);
        #[cfg(feature = "mcp")]
        rebind_mcp_manager(&mut self.ui.mcp_manager, self.ui.cfg, &self.ui.workspace).await;
        self.run.agent = Some(
            self.ui
                .agent_build_ctx()
                .rebuild_agent(&self.ui.session.model, self.slash.reasoning_enabled)
                .await,
        );
        render_session(
            &mut self.renderer,
            self.ui.session,
            self.ui.cli,
            self.ui.cfg,
            self.ui.context,
        )?;
        Ok(())
    }

    #[cfg(not(feature = "git-worktree"))]
    async fn handle_worktree_auto_merge(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The production merge cleanup boundary: every UI workspace owner settles before deletion.
#[cfg(feature = "git-worktree")]
#[allow(clippy::type_complexity)]
pub(crate) async fn retire_workspace_owners_and_complete_merge(
    run: &mut AgentRunState,
    btw_abort: &mut Vec<(
        u32,
        tokio::task::AbortHandle,
        tokio::task::JoinHandle<()>,
        Arc<crate::agent::runner::AgentWorkScope>,
    )>,
    btw_inflight: &mut usize,
    prebuild: &mut Option<super::prebuild::AgentPrebuild>,
    #[cfg(feature = "mcp")] mcp_manager: &mut Option<crate::extras::mcp::McpClientManager>,
    state: &mut crate::extras::git_worktree::MergeState,
    timeout: std::time::Duration,
) -> anyhow::Result<Result<(), String>> {
    run.retire(timeout).await?;
    for (_, _, task, scope) in btw_abort.drain(..) {
        retire_scoped_task(task, scope, "side-question", timeout).await?;
    }
    *btw_inflight = 0;
    if let Some(prebuild) = prebuild.take() {
        prebuild.retire(timeout).await?;
    }
    #[cfg(feature = "mcp")]
    if let Some(manager) = mcp_manager.take() {
        manager.shutdown().await;
    }
    Ok(crate::extras::git_worktree::complete_merge(state).await)
}

pub(crate) async fn retire_scoped_task(
    task: tokio::task::JoinHandle<()>,
    scope: std::sync::Arc<crate::agent::runner::AgentWorkScope>,
    label: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    scope.cancellation_handle().cancel();
    task.abort();
    tokio::time::timeout(timeout, async {
        let _ = task.await;
        scope.wait_idle().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out retiring the {label} workspace"))
}

struct AutoTriggerTurn {
    prompt: String,
    history: std::sync::Arc<[rig::completion::Message]>,
    pending_turn: PendingMainTurn,
}

impl AutoTriggerTurn {
    fn prepare(session: &Session, prompt: &str, keep_recent_tool_results: usize) -> Self {
        Self {
            prompt: prompt.to_string(),
            history: crate::agent::runner::convert_history_shared_with_tool_result_retention(
                session,
                keep_recent_tool_results,
            ),
            pending_turn: PendingMainTurn::capture(session, prompt),
        }
    }

    fn into_runner_inputs(
        self,
    ) -> (
        String,
        std::sync::Arc<[rig::completion::Message]>,
        PendingMainTurn,
    ) {
        (self.prompt, self.history, self.pending_turn)
    }
}

#[cfg(test)]
mod initial_turn_tests {
    use rig::completion::Message;

    use super::AutoTriggerTurn;
    use crate::session::{MessageRole, Session};
    use crate::ui::state::{AgentRunState, PendingMainTurn};

    #[test]
    fn initial_turn_keeps_current_prompt_separate_until_runner_starts() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        let turn = AutoTriggerTurn::prepare(
            &session,
            "current prompt",
            crate::session::DEFAULT_KEEP_RECENT_TOOL_RESULTS,
        );
        let (prompt, history, pending_turn) = turn.into_runner_inputs();

        assert_eq!(prompt, "current prompt");
        assert!(history.is_empty());
        assert!(session.messages.is_empty());

        let mut run = AgentRunState::default();
        crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[0].content, "current prompt");
        assert_eq!(
            run.pending_turn.as_ref().map(PendingMainTurn::prompt),
            Some("current prompt")
        );
    }

    #[test]
    fn resumed_history_precedes_current_prompt_without_duplication() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        session.add_message(MessageRole::User, "prior question");
        session.add_message(MessageRole::Assistant, "prior answer");

        let turn = AutoTriggerTurn::prepare(
            &session,
            "new question",
            crate::session::DEFAULT_KEEP_RECENT_TOOL_RESULTS,
        );
        let (prompt, history, pending_turn) = turn.into_runner_inputs();

        assert_eq!(prompt, "new question");
        assert_eq!(
            history.as_ref(),
            vec![
                Message::user("prior question"),
                Message::assistant("prior answer")
            ]
        );
        assert_eq!(session.messages.len(), 2);

        let mut run = AgentRunState::default();
        crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[2].role, MessageRole::User);
        assert_eq!(session.messages[2].content, "new question");
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|message| message.content == "new question")
                .count(),
            1
        );
    }

    #[test]
    fn failed_turn_rollback_restores_prompt_partial_state_and_undo() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        session.add_message(MessageRole::User, "prior question");
        session.add_message(MessageRole::Assistant, "prior answer");
        session.add_message(MessageRole::User, "rewound question");
        assert_eq!(session.rewind_to(2), 1);
        session.set_calibration(800, 40);
        let mut expected = session.clone();
        let mut run = AgentRunState::default();

        let pending_turn = PendingMainTurn::capture(&session, "retry me");
        crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
        session.add_message(MessageRole::Assistant, "partial response");
        session.add_tool_call("read", &serde_json::json!({"path": "secret.txt"}));
        session.add_tool_result("read", "partial tool output");
        session.total_input_tokens = 99;
        session.total_output_tokens = 17;
        session.total_cached_input_tokens = 23;
        session.total_cache_creation_input_tokens = 5;
        session.total_cost = 1.25;
        session
            .permission_allowlist
            .push(crate::session::PermissionAllowEntry {
                tool: "read".into(),
                pattern: "secret.txt".into(),
            });
        expected.total_input_tokens = 99;
        expected.total_output_tokens = 17;
        expected.total_cached_input_tokens = 23;
        expected.total_cache_creation_input_tokens = 5;
        expected.total_cost = 1.25;
        expected.permission_allowlist = session.permission_allowlist.clone();
        assert!(
            session
                .read_tracker
                .track_read("secret.txt", 0, 100)
                .is_none()
        );
        assert!(
            session
                .read_tracker
                .track_read("secret.txt", 0, 100)
                .is_some()
        );
        let restored = crate::ui::rollback_pending_main_turn(&mut run, &mut session);

        assert_eq!(restored.as_deref(), Some("retry me"));
        assert!(run.pending_turn.is_none());
        assert!(
            session
                .read_tracker
                .track_read("secret.txt", 0, 100)
                .is_none()
        );
        assert_eq!(
            serde_json::to_value(&session).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(session.redo(), expected.redo());
        assert_eq!(
            serde_json::to_value(&session).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
    }

    #[test]
    fn cancelled_turn_restores_the_prompt_and_pre_turn_transcript() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        session.add_message(MessageRole::User, "prior question");
        session.add_message(MessageRole::Assistant, "prior answer");
        let expected = serde_json::to_value(&session).unwrap();
        let mut run = AgentRunState::default();
        let pending_turn = PendingMainTurn::capture(&session, "cancel me");
        crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);

        let restored = crate::ui::rollback_pending_main_turn(&mut run, &mut session);

        assert_eq!(restored.as_deref(), Some("cancel me"));
        assert_eq!(serde_json::to_value(&session).unwrap(), expected);
        assert!(run.pending_turn.is_none());
    }

    #[test]
    fn interrupted_turn_with_progress_is_preserved_and_protocol_complete() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        session.add_message(MessageRole::User, "prior question");
        session.add_message(MessageRole::Assistant, "prior answer");
        let mut run = AgentRunState::default();
        let pending_turn = PendingMainTurn::capture(&session, "make the edit");
        crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
        session.add_tool_call_with_id(
            "interrupted-call",
            "edit",
            &serde_json::json!({"path": "src/main.rs"}),
        );
        run.response_buf = "partial explanation".to_string();

        assert!(crate::ui::preserve_pending_main_turn_progress(
            &mut run,
            &mut session
        ));

        assert!(run.pending_turn.is_some());
        assert_eq!(
            session
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            [
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::User,
                MessageRole::ToolCall,
                MessageRole::ToolResult,
                MessageRole::Assistant,
            ]
        );
        let synthetic = &session.messages[4];
        assert_eq!(synthetic.tool_call_id.as_deref(), Some("interrupted-call"));
        assert!(
            synthetic
                .content
                .contains(crate::agent::runner::UNKNOWN_TOOL_OUTCOME)
        );
        assert_eq!(session.messages[5].content, "partial explanation");
    }

    #[test]
    fn interrupted_turn_does_not_duplicate_a_completed_tool_result() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        let mut run = AgentRunState::default();
        let pending_turn = PendingMainTurn::capture(&session, "inspect");
        crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
        session.add_tool_call_with_id("completed-call", "read", &serde_json::json!({}));
        session.add_tool_result_with_id("completed-call", "read", "done");

        assert!(crate::ui::preserve_pending_main_turn_progress(
            &mut run,
            &mut session
        ));
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|message| message.role == MessageRole::ToolResult)
                .count(),
            1
        );
    }

    #[test]
    fn zero_progress_failure_remains_eligible_for_rollback() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        let mut run = AgentRunState::default();
        let pending_turn = PendingMainTurn::capture(&session, "retry me");
        crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);

        assert!(!crate::ui::preserve_pending_main_turn_progress(
            &mut run,
            &mut session
        ));
        assert_eq!(
            crate::ui::rollback_pending_main_turn(&mut run, &mut session).as_deref(),
            Some("retry me")
        );
        assert!(session.messages.is_empty());
    }

    #[test]
    fn presentation_failure_preserves_the_observed_turn_trace() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        let mut run = AgentRunState::default();
        let pending_turn = PendingMainTurn::capture(&session, "inspect");
        crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
        run.turn_trace.push("→ read src/main.rs".into());

        assert!(crate::ui::preserve_pending_main_turn_progress(
            &mut run,
            &mut session
        ));
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert!(
            session.messages[1]
                .content
                .contains("Interrupted turn progress")
        );
        assert!(session.messages[1].content.contains("read src/main.rs"));
    }

    #[cfg(feature = "multimodal")]
    #[test]
    fn failed_turn_restores_moved_pending_media_without_snapshot_clone() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        session
            .pending_media
            .push(crate::extras::multimodal::MediaAttachment::Image {
                path: std::path::PathBuf::from("attachment.png"),
                data: vec![1, 2, 3, 4],
                mime: "image/png".into(),
            });
        let mut pending = PendingMainTurn::capture(&session, "describe attachment");
        assert_eq!(pending.take_pending_media(&mut session).len(), 1);
        assert!(session.pending_media.is_empty());
        let mut run = AgentRunState::default();
        crate::ui::mark_main_turn_started(&mut session, &mut run, pending);

        crate::ui::rollback_pending_main_turn(&mut run, &mut session).unwrap();

        assert_eq!(session.pending_media.len(), 1);
        assert_eq!(session.pending_media[0].size(), 4);
    }
}

#[cfg(test)]
mod mid_turn_pressure_tests {
    use super::{MidTurnPressureAction, mid_turn_pressure_action};

    #[test]
    fn partial_reconciliation_preserves_relief_latch_until_complete_snapshot() {
        let first = mid_turn_pressure_action(false, true, true);
        assert_eq!(first, MidTurnPressureAction::Compact);
        let awaiting_relief = true;

        let partial = mid_turn_pressure_action(awaiting_relief, false, false);
        assert_eq!(partial, MidTurnPressureAction::Ignore);

        let still_over = mid_turn_pressure_action(awaiting_relief, true, true);
        assert_eq!(still_over, MidTurnPressureAction::StopContextExhausted);
    }
}

/// Release a saturated input sender without closing the shared completion
/// channel. Preserve drained events in their original order for the next UI
/// iteration. Once stopped, the reader can finish at most its current event.
fn pause_input_reader(
    running: &AtomicBool,
    handle: &mut Option<std::thread::JoinHandle<()>>,
    receiver: &mut mpsc::Receiver<UserEvent>,
    deferred: &mut std::collections::VecDeque<UserEvent>,
) {
    running.store(false, Ordering::Relaxed);
    if let Some(thread) = handle.take() {
        while !thread.is_finished() {
            if let Ok(event) = receiver.try_recv() {
                deferred.push_back(event);
            }
            // Yield to the reader even if background producers refill the queue.
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = thread.join();
    }
}

/// Slash commands that read the tty synchronously while the terminal is
/// suspended: `/tutor` runs a pager, `/init` (without `force`) asks y/N on
/// stdin. The event thread must be paused around them.
fn slash_command_needs_tty(text: &str) -> bool {
    let mut parts = text.split_whitespace();
    match parts.next() {
        Some("/tutor") => true,
        Some("/init") => parts.next() != Some("force"),
        _ => false,
    }
}

#[cfg(test)]
mod slash_tty_tests {
    use super::slash_command_needs_tty;

    #[test]
    fn slash_command_needs_tty_matches_only_stdin_consumers() {
        assert!(slash_command_needs_tty("/tutor"));
        assert!(slash_command_needs_tty("/init"));
        assert!(slash_command_needs_tty("  /init  "));
        assert!(!slash_command_needs_tty("/init force"));
        assert!(!slash_command_needs_tty("/undo"));
        assert!(!slash_command_needs_tty("/memory editor"));
        assert!(!slash_command_needs_tty(""));
    }
}

#[cfg(test)]
mod input_reader_lifecycle_tests {
    use super::*;

    #[test]
    fn saturated_reader_stop_preserves_order_and_background_senders() {
        let (sender, mut receiver) = mpsc::channel(1);
        let background = sender.clone();
        #[cfg(feature = "loop")]
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        #[cfg(feature = "loop")]
        let mut run = AgentRunState::default();
        let mut deferred = std::collections::VecDeque::from([UserEvent::Paste("earlier".into())]);
        // Exercise a second reader on the same event channel after a handoff.
        for round in 0..2 {
            #[cfg(feature = "loop")]
            let operation = crate::extras::r#loop::validation::start(
                &crate::sandbox::Sandbox::new(false, "bwrap"),
                "echo validated",
            );
            #[cfg(feature = "loop")]
            let operation_id = run.begin_validation(operation.cancellation());
            sender
                .blocking_send(UserEvent::Paste(format!("queued-{round}")))
                .unwrap();
            let running = Arc::new(AtomicBool::new(true));
            let reader_sender = sender.clone();
            let reader_running = running.clone();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let mut handle = Some(std::thread::spawn(move || {
                entered_tx.send(()).unwrap();
                reader_sender
                    .blocking_send(UserEvent::Paste(format!("reader-{round}")))
                    .unwrap();
                assert!(!reader_running.load(Ordering::Relaxed));
            }));
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            pause_input_reader(&running, &mut handle, &mut receiver, &mut deferred);
            assert!(handle.is_none());
            assert!(!receiver.is_closed());
            while let Ok(event) = receiver.try_recv() {
                deferred.push_back(event);
            }
            // A completion producer captured before the handoff is still live.
            #[cfg(feature = "loop")]
            let completion = {
                let result = runtime.block_on(operation.wait());
                assert!(result.succeeded());
                UserEvent::LoopValidationDone(crate::event::LoopValidationEvent {
                    operation_id,
                    response: "completed".into(),
                    summary: format!("completion-{round}"),
                    result,
                })
            };
            #[cfg(not(feature = "loop"))]
            let completion = UserEvent::LinkOpenFailed(format!("completion-{round}"));
            background.blocking_send(completion).unwrap();
            let completion = receiver.blocking_recv().unwrap();
            #[cfg(feature = "loop")]
            if let UserEvent::LoopValidationDone(event) = &completion {
                assert!(run.complete_validation(event.operation_id));
                assert!(!run.validation_active());
            }
            deferred.push_back(completion);
            pause_input_reader(&running, &mut handle, &mut receiver, &mut deferred);
        }
        let actual: Vec<_> = deferred
            .into_iter()
            .map(|event| match event {
                UserEvent::Paste(text) | UserEvent::LinkOpenFailed(text) => text,
                #[cfg(feature = "loop")]
                UserEvent::LoopValidationDone(event) => event.summary,
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(
            actual,
            [
                "earlier",
                "queued-0",
                "reader-0",
                "completion-0",
                "queued-1",
                "reader-1",
                "completion-1"
            ]
        );
    }
}
