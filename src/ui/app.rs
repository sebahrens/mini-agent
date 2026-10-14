#[cfg(feature = "mcp")]
#[path = "mcp_admin_app.rs"]
mod mcp_admin_app;

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
use crate::session::{GitStatus, Session};
use crate::ui::event_handler;
use crate::ui::events::{render_session, sanitize_output};
use crate::ui::input::{InputEditor, Picker};
use crate::ui::permission_handler::{PermissionEvent, PermissionState};
use crate::ui::pickers::rewind::RewindOutcome;
use crate::ui::question_handler::{QuestionEvent, QuestionState};
use crate::ui::renderer::{
    self as renderer_mod, ChainPrompt, ClipboardCopyOutcome, Renderer, SelectionPoint,
    copy_to_clipboard, read_from_clipboard,
};
use crate::ui::slash::{apply_prompt_model, handle_compress};
use crate::ui::state::{
    AgentRunState, BtwStats, ChainState, PendingMainTurn, SlashState, UiContext,
};
use crate::ui::terminal::TerminalGuard;

#[cfg(feature = "git-worktree")]
use super::apply_current_prompt_mode;
#[cfg(feature = "advisor")]
use super::handle_human_handoff;
#[cfg(all(feature = "mcp", feature = "git-worktree"))]
use super::rebind_mcp_manager;
use super::{
    C_AGENT, C_BTW, C_ERROR, C_PERM, C_TOOL, PrebuildPayload, apply_prompt_mode,
    classify_submission, mid_turn_compact_and_respawn, pending_main_turn_has_progress,
    preserve_pending_main_turn_progress, record_started_main_turn, refresh_display,
    rollback_pending_main_turn, spawn_event_thread, start_main_run, stop_turn_context_exhausted,
};

const TURN_TRACE_MAX: usize = 64;
const BTW_MAX_INFLIGHT: usize = 4;

/// Pricing belongs to the side request, including when the main model changes
/// before its completion. IDs are never reused and settlement consumes once.
#[derive(Default)]
struct BtwPricingSnapshots {
    next_id: u32,
    pending: std::collections::HashMap<u32, crate::print::HeadlessPricing>,
}

impl BtwPricingSnapshots {
    fn start(&mut self, pricing: crate::print::HeadlessPricing) -> Option<u32> {
        if self.pending.len() >= BTW_MAX_INFLIGHT {
            return None;
        }
        let next = self.next_id.checked_add(1)?;
        let id = self.next_id;
        self.next_id = next;
        self.pending.insert(id, pricing);
        Some(id)
    }

    fn take(&mut self, id: u32) -> Option<crate::print::HeadlessPricing> {
        self.pending.remove(&id)
    }

    fn cancel_all(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;

    pub(super) fn fixture_app(ui: UiContext<'_>) -> App<'_> {
        let (git_status_tx, git_status_rx) = mpsc::channel(1);
        let (btw_tx, btw_rx) = mpsc::channel(1);
        let (title_tx, title_rx) = mpsc::channel(1);
        let (user_tx, user_rx) = mpsc::channel(1);
        App {
            ui,
            run: AgentRunState::default(),
            chain: ChainState::default(),
            slash: SlashState {
                show_reasoning: false,
                reasoning_enabled: false,
                todo_tools_enabled: false,
            },
            renderer: Renderer::for_app_test().unwrap(),
            input: InputEditor::new(),
            last_branch_check: std::time::Instant::now(),
            git_status_tx,
            git_status_rx,
            git_status_in_flight: false,
            ask_rx: None,
            permission_prompt: None,
            question_prompt: None,
            #[cfg(feature = "advisor")]
            handoff_rx: None,
            btw_tx,
            btw_rx,
            title_tx,
            title_rx,
            title_requested_for: None,
            title_generation: 0,
            title_work: None,
            btw_abort: Vec::new(),
            btw_inflight: 0,
            btw_pricing: BtwPricingSnapshots::default(),
            btw_total_cost: 0.0,
            btw_total_in: 0,
            btw_total_out: 0,
            user_tx,
            user_rx,
            tui_log_rx: None,
            deferred_user_events: Default::default(),
            running: Arc::new(AtomicBool::new(false)),
            event_handle: None,
            prebuild: None,
            terminal_guard: TerminalGuard::detached_for_test(),
            #[cfg(feature = "mcp")]
            mcp_admin: None,
            model_cache_generation: 0,
        }
    }
}

#[cfg(test)]
mod dot_command_tests {
    use super::test_support::fixture_app;
    use super::*;

    fn transcript(app: &App<'_>) -> Vec<String> {
        let feed = app.renderer.feed();
        (0..feed.block_count())
            .filter_map(|index| feed.block_text(index).map(str::to_owned))
            .collect()
    }

    #[tokio::test]
    async fn actual_dot_handler_preserves_literals_and_echoes_one_shot_once() {
        let root = crate::tests::OwnedTestDirectory::new("dot-command-handler");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_session: true,
            ..Default::default()
        };
        let cfg = crate::config::Config {
            prompt_to_model: None,
            ..Default::default()
        };
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        context
            .prompts
            .insert("plan".into(), "Test plan prompt".into());
        let original = context.active_selection();
        let mut session = Session::new("anthropic", "test-model", 128_000, "");
        let ui = UiContext {
            cli: &cli,
            cfg: &cfg,
            session: &mut session,
            context: &mut context,
            workspace,
            client: crate::provider::create_client(
                "anthropic",
                Some("unused-test-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox: crate::sandbox::Sandbox::new(false, "none"),
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        let mut app = fixture_app(ui);
        for literal in [
            ".gitignore is wrong",
            "  .gitignore is wrong",
            "...continue",
            ".file.md",
            "quoted .plan foo",
        ] {
            let mut text = compact_str::CompactString::new(literal);
            assert!(
                !app.handle_dot_command(&mut text).await.unwrap(),
                "{literal}"
            );
            assert_eq!(text, literal);
            assert_eq!(app.ui.context.active_selection(), original);
            assert!(
                transcript(&app).is_empty(),
                "literal handling must defer its single echo to submission"
            );
        }
        for (shortcut, message) in [
            (".plan foo", "foo"),
            ("  .plan foo", "foo"),
            (".plan /help", "/help"),
            (".plan !literal", "!literal"),
        ] {
            app.renderer.clear_content().unwrap();
            let mut text = compact_str::CompactString::new(shortcut);
            assert!(!app.handle_dot_command(&mut text).await.unwrap());
            assert_eq!(text, message);
            assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
            assert_eq!(app.chain.dot_prompt_restore, Some(original.clone()));
            assert!(transcript(&app).is_empty());
            app.echo_main_submission(&text).unwrap();
            assert_eq!(
                transcript(&app)
                    .iter()
                    .filter(|line| line.starts_with("> "))
                    .cloned()
                    .collect::<Vec<_>>(),
                vec![format!("> {message}")]
            );
            app.ui.context.restore_selection(original.clone());
        }
        app.renderer.clear_content().unwrap();
        let mut typo = compact_str::CompactString::new(".unknownprompt");
        assert!(app.handle_dot_command(&mut typo).await.unwrap());
        assert!(
            transcript(&app)
                .iter()
                .any(|line| line == "error: unknown prompt 'unknownprompt'")
        );
        assert_eq!(app.ui.context.active_selection(), original);
        let mut picker = compact_str::CompactString::new("  .");
        assert!(app.handle_dot_command(&mut picker).await.unwrap());
        assert_eq!(app.input.buffer, ".");
        let mut switch = compact_str::CompactString::new(" .plan  ");
        assert!(app.handle_dot_command(&mut switch).await.unwrap());
        assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
        assert!(
            transcript(&app)
                .iter()
                .any(|line| line == "switched to prompt 'plan'")
        );
        assert!(
            app.ui.session.messages.is_empty(),
            "command-only shortcuts never submit an empty provider message"
        );
    }
}

#[cfg(test)]
mod btw_pricing_tests {
    use super::test_support::fixture_app;
    use super::*;

    #[tokio::test]
    async fn quick_model_price_presence_actual_app_usage_charges_paid_alias() {
        let root = crate::tests::OwnedTestDirectory::new("quick-model-price-presence-app");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            ("ZS_CONFIG_DIR", Some(root.path().as_os_str().to_owned())),
            ("ZS_DATA_DIR", Some(root.path().as_os_str().to_owned())),
            ("ZS_STATE_DIR", Some(root.path().as_os_str().to_owned())),
        ]);
        let cli = crate::cli::Cli::default();
        let cfg: crate::config::Config =
            toml::from_str("[quick_models.paid]\nprovider='anthropic'\nmodel='claude-sonnet-5-5'")
                .unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("anthropic", "claude-fable-5-1", 1_000_000, "");
        session
            .set_input_output_prices(cfg.quick_models.as_ref().unwrap()["paid"].resolved_prices());
        let ui = UiContext {
            cli: &cli,
            cfg: &cfg,
            session: &mut session,
            context: &mut context,
            workspace,
            client: crate::provider::create_client(
                "anthropic",
                Some("unused-test-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox: crate::sandbox::Sandbox::new(false, "none"),
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        let mut app = fixture_app(ui);
        let expected =
            crate::config::Config::catalog_input_output_cost("anthropic", "claude-sonnet-5-5")
                .unwrap();
        app.handle_agent_event(AgentEvent::UsageDelta {
            usage: crate::event::UsageDelta {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                ..Default::default()
            },
            context_complete: false,
            awaits_compaction_decision: false,
        })
        .await
        .unwrap();
        assert_eq!(app.ui.session.total_cost, expected.0 + expected.1);
        assert_eq!(app.ui.session.known_input_output_prices(), Some(expected));
    }

    #[test]
    fn actual_btw_event_handler_charges_snapshot_and_ignores_duplicate_or_cancelled_ids() {
        let root = crate::tests::OwnedTestDirectory::new("btw-pricing-handler");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            ("ZS_CONFIG_DIR", Some(root.path().as_os_str().to_owned())),
            ("ZS_DATA_DIR", Some(root.path().as_os_str().to_owned())),
            ("ZS_STATE_DIR", Some(root.path().as_os_str().to_owned())),
        ]);
        let cli = crate::cli::Cli::default();
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("anthropic", "claude-fable-5-1", 1_000_000, "");
        session.input_token_cost = 10.0;
        session.output_token_cost = 50.0;
        let ui = UiContext {
            cli: &cli,
            cfg: &cfg,
            session: &mut session,
            context: &mut context,
            workspace,
            client: crate::provider::create_client(
                "anthropic",
                Some("unused-test-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox: crate::sandbox::Sandbox::new(false, "none"),
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        let mut app = fixture_app(ui);
        let id = app
            .btw_pricing
            .start(crate::print::HeadlessPricing::from_session(
                app.ui.session,
                app.ui.cfg,
            ))
            .unwrap();
        app.btw_inflight = 1;
        app.ui.session.model = "claude-opus-5-5".into();
        app.ui.session.input_token_cost = 4.0;
        app.ui.session.output_token_cost = 20.0;
        let event = BtwEvent::Done {
            id,
            response: "answer".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 1_000_000,
            cache_creation_input_tokens: 0,
        };
        app.handle_btw_event(event.clone()).unwrap();
        assert_eq!(app.btw_total_cost, 0.25);
        assert_eq!(app.btw_inflight, 0);
        assert!(app.btw_pricing.pending.is_empty());
        app.handle_btw_event(event).unwrap();
        assert_eq!(
            app.btw_total_cost, 0.25,
            "duplicate Done cannot charge again"
        );
        let cancelled = app
            .btw_pricing
            .start(crate::print::HeadlessPricing::from_session(
                app.ui.session,
                app.ui.cfg,
            ))
            .unwrap();
        app.btw_pricing.cancel_all();
        let active = app
            .btw_pricing
            .start(crate::print::HeadlessPricing::from_session(
                app.ui.session,
                app.ui.cfg,
            ))
            .unwrap();
        app.btw_inflight = 1;
        app.handle_btw_event(BtwEvent::Done {
            id: cancelled,
            response: "stale".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 1_000_000,
            cache_creation_input_tokens: 0,
        })
        .unwrap();
        assert_eq!(app.btw_total_cost, 0.25);
        assert_eq!(
            app.btw_inflight, 1,
            "stale result cannot settle the active request"
        );
        app.handle_btw_event(BtwEvent::Error {
            id: active,
            message: "fixture failure".into(),
        })
        .unwrap();
        assert_eq!(app.btw_inflight, 0);
        assert!(app.btw_pricing.pending.is_empty());
        app.handle_btw_event(BtwEvent::Done {
            id: active,
            response: "stale after error".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 1_000_000,
            cache_creation_input_tokens: 0,
        })
        .unwrap();
        assert_eq!(app.btw_total_cost, 0.25);
        app.ui.session.input_token_cost = 0.0;
        app.ui.session.output_token_cost = 0.0;
        app.ui
            .session
            .set_cache_prices(crate::pricing::CachePrices {
                read: Some(1.0),
                write: None,
                write_1h: None,
                split_required: false,
            });
        let cache_only = app
            .btw_pricing
            .start(crate::print::HeadlessPricing::from_session(
                app.ui.session,
                app.ui.cfg,
            ))
            .unwrap();
        app.btw_inflight = 1;
        app.ui.session.input_token_cost = 4.0;
        app.ui
            .session
            .set_cache_prices(crate::pricing::CachePrices {
                read: Some(0.2),
                write: Some(5.0),
                write_1h: None,
                split_required: false,
            });
        app.handle_btw_event(BtwEvent::Done {
            id: cache_only,
            response: "cache-only answer".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 1_000_000,
            cache_creation_input_tokens: 0,
        })
        .unwrap();
        assert_eq!(
            app.btw_total_cost, 1.25,
            "zero-base absolute cache pricing belongs to the request snapshot"
        );
        assert_eq!(app.btw_inflight, 0);
        assert_eq!(
            app.ui.session.total_cost, 0.0,
            "side questions stay outside main accounting"
        );
    }

    #[test]
    fn btw_pricing_snapshot_survives_model_switch_and_settles_each_request_once() {
        let cfg = crate::config::Config::default();
        let mut active = Session::new("anthropic", "claude-fable-5-1", 1_000_000, "");
        active.input_token_cost = 10.0;
        active.output_token_cost = 50.0;
        let mut registry = BtwPricingSnapshots::default();
        let id = registry
            .start(crate::print::HeadlessPricing::from_session(&active, &cfg))
            .unwrap();
        active.model = "claude-opus-5-5".into();
        active.input_token_cost = 4.0;
        active.output_token_cost = 20.0;
        let usage = rig::completion::Usage {
            cached_input_tokens: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            crate::print::HeadlessPricing::from_session(&active, &cfg).cost(usage),
            0.2
        );
        assert_eq!(registry.take(id).unwrap().cost(usage), 0.25);
        assert!(
            registry.take(id).is_none(),
            "duplicate completion cannot charge twice"
        );

        let retired = registry
            .start(crate::print::HeadlessPricing::from_session(&active, &cfg))
            .unwrap();
        registry.cancel_all();
        assert!(registry.pending.is_empty());
        let current = registry
            .start(crate::print::HeadlessPricing::from_session(&active, &cfg))
            .unwrap();
        assert_ne!(current, retired);
        assert!(
            registry.take(retired).is_none(),
            "stale cancelled/retired result cannot charge a current request"
        );
        assert_eq!(registry.take(current).unwrap().cost(usage), 0.2);
    }

    #[test]
    fn btw_pricing_retention_is_bounded_and_ids_never_wrap() {
        let session = Session::new("anthropic", "unknown", 1_000, "");
        let pricing = crate::print::HeadlessPricing::from_session(
            &session,
            &crate::config::Config::default(),
        );
        let mut registry = BtwPricingSnapshots::default();
        let ids: Vec<_> = (0..BTW_MAX_INFLIGHT)
            .map(|_| registry.start(pricing).unwrap())
            .collect();
        assert!(registry.start(pricing).is_none());
        assert_eq!(registry.pending.len(), BTW_MAX_INFLIGHT);
        assert!(registry.take(ids[0]).is_some());
        let next = registry.start(pricing).unwrap();
        assert!(!ids.contains(&next));
        registry.cancel_all();
        registry.next_id = u32::MAX;
        assert!(registry.start(pricing).is_none());
        assert!(registry.pending.is_empty());
    }
}

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
            let quit = matches!(event, UserEvent::Quit);
            deferred.push_back(event);
            if quit {
                return WorktreePromptChoice::Abort;
            }
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

/// Whether releasing the left button should copy the transcript selection.
/// Only a real drag copies; a plain click (no movement, one line) does not.
fn mouse_up_copies<P: PartialEq>(dragged: bool, start: Option<P>, end: Option<P>) -> bool {
    match (start, end) {
        (Some(start), Some(end)) => dragged || start != end,
        _ => false,
    }
}

#[cfg(test)]
mod mouse_selection_tests {
    use super::{SelectionPoint, mouse_up_copies};

    #[test]
    fn plain_click_does_not_copy_but_a_drag_does() {
        let at = |line, col| Some(SelectionPoint::new(line, col));
        assert!(!mouse_up_copies(false, at(4, 3), at(4, 3)));
        assert!(mouse_up_copies(true, at(4, 3), at(4, 3)));
        assert!(mouse_up_copies(false, at(4, 3), at(6, 0)));
        assert!(mouse_up_copies(false, at(4, 3), at(4, 9)));
        assert!(mouse_up_copies(true, at(6, 0), at(4, 3)));
        assert!(!mouse_up_copies::<SelectionPoint>(true, None, None));
    }
}

/// The lazygit shortcut, Ctrl+O. It used to be Ctrl+H, but many terminals
/// send `^H` for Backspace and crossterm cannot tell the two apart, so Ctrl+H
/// stays backspace in the editor and pickers.
fn is_lazygit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('o' | 'O'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptScroll {
    Top,
    Bottom,
}

/// Decide whether Home/End scroll the transcript instead of editing the
/// prompt. Ctrl+Home/Ctrl+End always scroll. Plain Home/End scroll only when
/// the input is empty (nothing to move through) or a picker is open (moving
/// the caret would desynchronise the picker query from the buffer); otherwise
/// they reach the editor and move the caret to the line start/end.
fn transcript_scroll_key(
    key: KeyEvent,
    input_empty: bool,
    picker_active: bool,
) -> Option<TranscriptScroll> {
    let target = match key.code {
        KeyCode::Home => TranscriptScroll::Top,
        KeyCode::End => TranscriptScroll::Bottom,
        _ => return None,
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    (ctrl || input_empty || picker_active).then_some(target)
}

#[cfg(test)]
mod home_end_tests {
    use super::*;

    #[test]
    fn home_end_edit_a_non_empty_prompt_and_scroll_otherwise() {
        let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        let ctrl_home = KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL);
        let ctrl_end = KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL);

        // Non-empty prompt, no picker: the editor gets the key.
        assert_eq!(transcript_scroll_key(home, false, false), None);
        assert_eq!(transcript_scroll_key(end, false, false), None);
        // Empty prompt: plain Home/End scroll the transcript.
        assert_eq!(
            transcript_scroll_key(home, true, false),
            Some(TranscriptScroll::Top)
        );
        assert_eq!(
            transcript_scroll_key(end, true, false),
            Some(TranscriptScroll::Bottom)
        );
        // An open picker keeps its query in sync by not moving the caret.
        assert_eq!(
            transcript_scroll_key(end, false, true),
            Some(TranscriptScroll::Bottom)
        );
        // Ctrl+Home/Ctrl+End always scroll.
        assert_eq!(
            transcript_scroll_key(ctrl_home, false, false),
            Some(TranscriptScroll::Top)
        );
        assert_eq!(
            transcript_scroll_key(ctrl_end, false, false),
            Some(TranscriptScroll::Bottom)
        );
        // Other keys are never transcript scrolls here.
        let page_up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(transcript_scroll_key(page_up, true, false), None);
    }
}

#[cfg(test)]
mod lazygit_key_tests {
    use super::*;

    #[test]
    fn lazygit_is_ctrl_o_and_ctrl_h_stays_backspace() {
        assert!(is_lazygit_key(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL
        )));
        for key in [
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('\u{8}'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
        ] {
            assert!(!is_lazygit_key(key), "{key:?}");
        }
        // Idle Ctrl+C clears a draft first; Ctrl+D forward-deletes in it.
        assert_eq!(idle_interrupt(false, false), IdleInterrupt::ClearDraft);
        assert_eq!(idle_interrupt(true, false), IdleInterrupt::EditorKey);
        assert_eq!(idle_interrupt(false, true), IdleInterrupt::Exit);
        assert_eq!(idle_interrupt(true, true), IdleInterrupt::Exit);
        let mut input = InputEditor::new();
        input.load_text("draft");
        input.discard_draft();
        assert_eq!(input.buffer, "");
        input.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(input.buffer, "draft", "Ctrl+Y recovers a cleared draft");
        input.set_cursor(0);
        input.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(input.buffer, "raft", "Ctrl+D deletes forward");

        // Nothing is warmed after /quit or /exit (mini-agent-bgg91).
        assert!(is_quit_command("/quit"));
        assert!(is_quit_command(" /exit now"));
        assert!(!is_quit_command("/quiet"));
        assert!(!is_quit_command("/model x"));

        // Ctrl+H reaches the editor as backspace.
        let mut input = InputEditor::new();
        input.load_text("ab");
        assert_eq!(
            input.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(input.buffer, "a");
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

/// What an idle Ctrl+C / Ctrl+D (nothing to interrupt) does to the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleInterrupt {
    /// Ctrl+C with a draft: clear it (recoverable with Ctrl+Y); a second
    /// press on the now empty input quits.
    ClearDraft,
    /// Ctrl+D with a draft: forward-delete in the editor, as in a shell.
    EditorKey,
    /// Empty input: quit.
    Exit,
}

pub(crate) fn idle_interrupt(is_ctrl_d: bool, draft_empty: bool) -> IdleInterrupt {
    match (draft_empty, is_ctrl_d) {
        (true, _) => IdleInterrupt::Exit,
        (false, true) => IdleInterrupt::EditorKey,
        (false, false) => IdleInterrupt::ClearDraft,
    }
}

/// `/quit` and `/exit` leave the app; nothing needs warming after them.
fn is_quit_command(text: &str) -> bool {
    matches!(text.split_whitespace().next(), Some("/quit" | "/exit"))
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

/// A background session-title call's answer (mini-agent-3wsib).
struct SessionTitleResult {
    generation: u64,
    session_id: compact_str::CompactString,
    title: anyhow::Result<compact_str::CompactString>,
}

struct SessionTitleWork {
    session_id: compact_str::CompactString,
    generation: u64,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    provider: compact_str::CompactString,
    pricing: crate::session::title::TitlePricing,
    ledger: crate::session::title::UsageLedger,
    work_scope: Arc<crate::agent::runner::AgentWorkScope>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for SessionTitleWork {
    fn drop(&mut self) {
        self.work_scope.cancellation_handle().cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
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
    ask_rx: Option<mpsc::Receiver<crate::permission::ask::UserRequest>>,
    permission_prompt: Option<PermissionState>,
    question_prompt: Option<QuestionState>,
    #[cfg(feature = "advisor")]
    handoff_rx: Option<crate::extras::advisor::HandoffReceiver>,

    btw_tx: mpsc::Sender<BtwEvent>,
    btw_rx: mpsc::Receiver<BtwEvent>,
    /// Generated session titles coming back from the background title call
    /// (mini-agent-3wsib), keyed by the session they were generated for.
    title_tx: mpsc::Sender<SessionTitleResult>,
    title_rx: mpsc::Receiver<SessionTitleResult>,
    /// The session a title was last requested for, so each session is asked
    /// about at most once per process whatever the outcome.
    title_requested_for: Option<compact_str::CompactString>,
    title_generation: u64,
    title_work: Option<SessionTitleWork>,
    btw_abort: Vec<(
        u32,
        tokio::task::AbortHandle,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<crate::agent::runner::AgentWorkScope>,
    )>,
    btw_inflight: usize,
    btw_pricing: BtwPricingSnapshots,
    btw_total_cost: f64,
    btw_total_in: u64,
    btw_total_out: u64,

    user_tx: mpsc::Sender<UserEvent>,
    user_rx: mpsc::Receiver<UserEvent>,
    tui_log_rx: Option<mpsc::Receiver<String>>,
    deferred_user_events: std::collections::VecDeque<UserEvent>,
    running: Arc<AtomicBool>,
    event_handle: Option<std::thread::JoinHandle<()>>,
    prebuild: Option<super::prebuild::AgentPrebuild>,
    terminal_guard: TerminalGuard,
    /// Last model-cache generation handed to the model picker.
    #[cfg(feature = "mcp")]
    mcp_admin: Option<Box<crate::ui::mcp_admin::State>>,
    model_cache_generation: u64,
}

#[cfg(test)]
mod reasoning_display_tests {
    use super::*;
    use crate::config::ReasoningDisplay;

    #[tokio::test]
    async fn actual_reasoning_controls_preserve_the_turn_and_persist_every_cycle() {
        let root = crate::tests::OwnedTestDirectory::new("reasoning-app-controls");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session =
            crate::session::Session::new("openrouter", "unused-test-model", 128_000, "");
        let client = crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            &cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap();
        let ui = UiContext::new(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            client,
            None,
            None,
            crate::sandbox::Sandbox::new(false, "none"),
            None,
        );
        let mut app = test_support::fixture_app(ui);
        app.slash.show_reasoning = true;
        app.run.is_running = true;
        app.input.buffer = "unsent draft".into();
        app.input.cursor = app.input.buffer.len();
        app.run.pending_inputs.push_back("queued request".into());
        let before = serde_json::to_value(&*app.ui.session).unwrap();
        for text in ["REASONING_PRIVATE_界", "🙂"] {
            app.handle_agent_event(AgentEvent::Reasoning(text.into()))
                .await
                .unwrap();
        }
        assert_eq!(app.renderer.feed().block_count(), 1);
        assert!(
            !app.renderer.feed().lines(100)[0]
                .text
                .contains("REASONING_PRIVATE")
        );
        let row = (0..app.renderer.visible_lines() as u16)
            .find(|row| app.renderer.buffer_line_at_row(*row) == Some(0))
            .unwrap();
        assert!(matches!(
            app.handle_user_event(UserEvent::MouseDown { row, col: 0 })
                .await
                .unwrap(),
            ControlFlow::Continue(())
        ));
        assert!(
            app.renderer.feed().lines(100)[1]
                .text
                .contains("REASONING_PRIVATE")
        );
        assert!(!app.renderer.selection_active);
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        for expected in [
            ReasoningDisplay::Full,
            ReasoningDisplay::Off,
            ReasoningDisplay::Collapsed,
            ReasoningDisplay::Full,
        ] {
            app.handle_key_event(key).await.unwrap();
            assert_eq!(app.renderer.feed().reasoning_display(), expected);
            let saved: crate::config::Config = toml::from_str(
                &std::fs::read_to_string(crate::config::config_file_path()).unwrap(),
            )
            .unwrap();
            assert_eq!(saved.resolve_reasoning_display(), expected);
        }
        assert!(app.run.agent.is_none());
        assert_eq!(serde_json::to_value(&*app.ui.session).unwrap(), before);
        assert_eq!(app.input.buffer, "unsent draft");
        assert_eq!(
            app.run.pending_inputs.front().map(String::as_str),
            Some("queued request")
        );
        assert!(app.run.is_running);
        app.handle_agent_event(AgentEvent::Token("earlier answer ".into()))
            .await
            .unwrap();
        app.handle_agent_event(AgentEvent::Reasoning("SECOND_THOUGHT".into()))
            .await
            .unwrap();
        // A malformed concurrently edited config leaves live preference and
        // transcript untouched, instead of reporting a successful save.
        std::fs::write(crate::config::config_file_path(), "broken = [").unwrap();
        app.handle_key_event(key).await.unwrap();
        assert_eq!(
            app.renderer.feed().reasoning_display(),
            ReasoningDisplay::Full
        );
        assert!(app.renderer.feed().lines(100).len() > 1);
        app.handle_agent_event(AgentEvent::Token("visible answer".into()))
            .await
            .unwrap();
        assert_eq!(app.run.response_buf, "earlier answer visible answer");
        assert!(
            (0..app.renderer.feed().block_count())
                .filter(|index| app.renderer.feed().is_streaming(*index))
                .count()
                <= 1
        );
        assert!(
            app.renderer.feed().lines(100)[0]
                .text
                .starts_with("▾ thought")
        );
        app.abort_main_run().await.unwrap();
        let complete = app.renderer.feed().lines(100);
        app.renderer
            .feed_mut()
            .tick_reasoning_at(std::time::Instant::now() + Duration::from_secs(10));
        assert!(Arc::ptr_eq(&complete, &app.renderer.feed().lines(100)));
        // Changing provider reasoning policy cannot reset the live display to
        // the immutable startup config after a saved Ctrl+R transition.
        app.slash.reasoning_enabled = true;
        for command in ["/reasoning off", "/reasoning on"] {
            app.run_slash_command(command).await.unwrap();
            assert_eq!(
                app.renderer.feed().reasoning_display(),
                ReasoningDisplay::Full
            );
            assert!(app.slash.show_reasoning);
        }
    }
}

impl<'a> App<'a> {
    pub(crate) async fn new(
        mut ui: UiContext<'a>,
        agent: Option<AnyAgent>,
        ask_rx: Option<mpsc::Receiver<crate::permission::ask::UserRequest>>,
        auto_trigger_msg: Option<String>,
        #[cfg(feature = "advisor")] handoff_rx: Option<crate::extras::advisor::HandoffReceiver>,
        #[cfg(feature = "hooks")] mut session_start_task: Option<tokio::task::JoinHandle<()>>,
    ) -> anyhow::Result<Self> {
        let terminal_guard = TerminalGuard::new(crate::ui::terminal::TerminalOptions {
            mouse_capture: ui.cfg.resolve_mouse_capture(),
            title_status: ui.cfg.resolve_terminal_title(),
        })?;

        // No input thread exists yet: retain every unrelated decoded event for
        // its sole owner instead of flushing or racing raw terminal input.
        let background = crate::ui::terminal::query_background(!ui.cli.no_color);

        ui.session.show_cost_always = ui.cfg.resolve_show_cost_always();
        crate::ui::statusline::init(ui.cfg);

        ui.session.refresh_git_branch();
        let last_branch_check = std::time::Instant::now();

        let mut renderer = Renderer::new()?;
        renderer.set_terminal_background(background.color);
        renderer.set_statusline_height(crate::ui::statusline::line_count());
        renderer.set_monochrome(ui.cli.no_color);
        renderer.set_title_status(ui.cfg.resolve_terminal_title());
        renderer.set_activity_signals(crate::ui::terminal::ActivitySignals {
            notify: ui.cfg.resolve_terminal_notify(),
            prompt_marks: ui.cfg.resolve_terminal_prompt_marks(),
        });
        renderer.set_chat_margin(ui.cfg.resolve_chat_left_margin());
        let theme_warning = crate::context::themes::apply_initial(
            ui.context,
            ui.cfg.colors.as_ref(),
            &mut renderer,
        );

        let mut input = InputEditor::new();
        input.set_file_workspace(ui.workspace.clone());
        input.set_monochrome(ui.cli.no_color);
        input.set_prompt_names(ui.context.prompts.keys().cloned().collect());
        input.set_custom_commands(
            crate::context::commands::load_for_workspace(&ui.workspace)
                .into_iter()
                .map(|(name, template)| (format!("/{name}"), template.description))
                .collect(),
        );
        #[cfg(feature = "agent-skills")]
        if let Ok(services) = ui.instruction_services()
            && let Ok(snapshot) = services.refresh(false).await
        {
            input.set_skill_commands(
                snapshot
                    .packages
                    .iter()
                    .filter(|(name, _)| !crate::ui::pickers::list::reserved_command(name))
                    .map(|(name, package)| {
                        (
                            format!("/{name}"),
                            format!(
                                "{} — {}",
                                package.origin,
                                skill_description(&package.record.description)
                            ),
                        )
                    })
                    .collect(),
            );
        }
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
        input.set_permission(ui.permission.clone());
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
        renderer
            .feed_mut()
            .set_reasoning_display(ui.cfg.resolve_reasoning_display());
        ui.session.reasoning_enabled = slash.reasoning_enabled;
        #[cfg(feature = "goal")]
        let goal_block =
            crate::extras::goal::prompt::goal_block(ui.session.goal_store.snapshot().as_ref());
        #[cfg(not(feature = "goal"))]
        let goal_block: Option<String> = None;
        let (overhead, parts) = crate::agent::builder::estimate_overhead_breakdown(
            ui.context,
            slash.reasoning_enabled,
            ui.cli,
            ui.cfg,
            &ui.sandbox,
            goal_block.as_deref(),
        );
        ui.session.overhead_tokens = overhead;
        ui.session.overhead_parts = parts;

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
        // The stderr warning is overdrawn once the TUI owns the screen; restate
        // the degraded sandbox in the chat before the first turn. The status
        // bar keeps a persistent `sandbox:off` segment for the whole session.
        if let Some(notice) = crate::permission::degraded_sandbox_notice_for(&ui.sandbox) {
            renderer.write_line(&format!("[sandbox] {notice}"), C_ERROR)?;
            renderer.write_line("", Color::White)?;
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
            // Non-blocking: an unreachable gateway must not stall startup.
            crate::ui::slash::warm_model_cache(&provider, is_custom, &ui.client);
            input.set_live_model_names(crate::ui::slash::cached_model_ids(
                &provider, is_custom, &ui.client,
            ));
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

        // Session/worktree rendering clears the feed, so show this only after
        // the final startup render and before any automatic prompt starts.
        if let Some(warning) = theme_warning {
            renderer.write_line(&sanitize_output(&warning), C_ERROR)?;
        }
        if let Some(question) = &ui.session.pending_question {
            renderer.write_line(&sanitize_output(&question.notice()), Color::Yellow)?;
        }
        if let Some(notice) = &ui.cfg.update_notice {
            renderer.write_line(&sanitize_output(notice), Color::DarkGrey)?;
            renderer.write_line("", Color::White)?;
        }
        #[cfg(feature = "subagents")]
        if let Some(notice) =
            crate::extras::subagents::with_config(|config| config.fallback_notice.clone())
                .ok()
                .flatten()
        {
            renderer.write_line(&sanitize_output(&notice), C_ERROR)?;
            renderer.write_line("", Color::White)?;
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
            run.question_turn = Some(crate::question::QuestionTurn::default());
            ui.session.pending_question = None;
            #[cfg(feature = "agent-skills")]
            {
                run.instruction_turn = Some(crate::extras::skills::session::InstructionTurn::new(
                    ui.instruction_services()?,
                    None,
                ));
            }
            let agent = run.agent.as_ref().unwrap().clone();
            #[cfg(feature = "agent-skills")]
            let agent = agent.with_instruction_turn(run.instruction_turn.clone());
            let runner = agent
                .with_spend_for_session(ui.session, ui.cfg, crate::spend::RequestKind::Turn)
                .with_question_turn(run.question_turn.clone())
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
        let event_handle = Some(super::spawn_event_thread_with_events(
            user_tx.clone(),
            running.clone(),
            background.pending,
        ));
        super::spawn_termination_listener(user_tx.clone());

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
                                McpClientManager::connect_config_in_binding(
                                    &cfg_clone,
                                    &workspace_clone,
                                )
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
        let (title_tx, title_rx) = mpsc::channel::<SessionTitleResult>(4);
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
            permission_prompt: None,
            question_prompt: None,
            #[cfg(feature = "advisor")]
            handoff_rx,
            btw_tx,
            btw_rx,
            title_tx,
            title_rx,
            title_requested_for: None,
            title_generation: 0,
            title_work: None,
            btw_abort: Vec::new(),
            btw_inflight: 0,
            btw_pricing: BtwPricingSnapshots::default(),
            btw_total_cost: 0.0,
            btw_total_in: 0,
            btw_total_out: 0,
            user_tx,
            user_rx,
            tui_log_rx: crate::logging::take_tui_diagnostics(),
            deferred_user_events: std::collections::VecDeque::new(),
            running,
            event_handle,
            prebuild,
            terminal_guard,
            #[cfg(feature = "mcp")]
            mcp_admin: None,
            model_cache_generation: crate::ui::slash::model_cache_generation(),
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
        let result = crate::startup::heap_future(|| self.run_inner()).await;
        #[cfg(feature = "mcp")]
        let mcp_retirement = self.retire_mcp_work().await;
        if result.is_err() {
            self.cancel_pending_permissions();
            if self.run.pending_turn.is_some() {
                self.fail_pending_main_turn().await;
            }
        }
        #[cfg(feature = "mcp")]
        {
            result.and(mcp_retirement)
        }
        #[cfg(not(feature = "mcp"))]
        {
            result
        }
    }

    async fn run_inner(&mut self) -> anyhow::Result<()> {
        loop {
            self.expire_abandoned_permission()?;
            self.drain_tui_diagnostics()?;
            #[cfg(feature = "git-worktree")]
            self.drain_git_observations()?;
            // Every adoption/rebuild path returns here, including a prompt that
            // awaited prebuild and deferred events that immediately continue.
            #[cfg(feature = "mcp")]
            if let Some(manager) = self.ui.mcp_manager.as_mut() {
                for notice in manager.take_notices() {
                    self.renderer
                        .write_line(&sanitize_output(&notice), C_ERROR)?;
                }
            }
            if (self.permission_prompt.is_none() && self.question_prompt.is_none())
                && let Some(event) = self.deferred_user_events.pop_front()
            {
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
                Some(message) = async { self.tui_log_rx.as_mut()?.recv().await } => {
                    for line in message.lines() {
                        self.renderer.write_line(&sanitize_output(line), C_ERROR)?;
                    }
                    self.drain_tui_diagnostics()?;
                }
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
                    // This measured large child is unrelated to input, but
                    // its inline storage inflated every event-loop poll.
                    crate::startup::heap_future(|| self.handle_agent_event(event)).await?;
                    // Streaming paints the chat directly and would leave an
                    // open picker erased until the next key; repaint it.
                    if self.input.picker.as_ref().is_some_and(|picker| picker.active()) {
                        self.refresh()?;
                    }
                }
                Some(ask_req) = async { self.ask_rx.as_mut()?.recv().await }, if (self.permission_prompt.is_none() && self.question_prompt.is_none()) => {
                    self.begin_user_request(ask_req)?;
                    self.refresh()?;
                }
                Some(bev) = self.btw_rx.recv() => {
                    self.handle_btw_event(bev)?;
                    self.refresh()?;
                }
                Some(titled) = self.title_rx.recv() => {
                    self.apply_session_title(titled).await?;
                    self.refresh()?;
                }
                _ = tokio::time::sleep_until(
                    self.renderer.notice_deadline().map_or_else(tokio::time::Instant::now, tokio::time::Instant::from_std)
                ), if self.renderer.notice_deadline().is_some() => {
                    // A transient notice expired; the redraw clears it.
                    self.refresh()?;
                }
                _ = tokio::time::sleep(Duration::from_millis(100)), if self.run.is_running => {
                    self.renderer.feed_mut().tick_reasoning_at(std::time::Instant::now());
                    if self.renderer.paint_pending_chat()?
                        && self.input.picker.as_ref().is_some_and(|picker| picker.active())
                    {
                        self.refresh()?;
                    }
                    self.renderer.tick_spinner()?;
                }
                // The @ file picker fills from a background walk; repaint as
                // results land so "scanning files..." never waits for a key.
                _ = tokio::time::sleep(Duration::from_millis(50)),
                    if self.input.picker.as_ref().is_some_and(Picker::is_loading) =>
                {
                    if self.input.picker.as_mut().is_some_and(Picker::poll_background) {
                        self.refresh()?;
                    }
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

            if self.permission_prompt.is_some() || self.question_prompt.is_some() {
                self.refresh()?;
            }

            #[cfg(feature = "advisor")]
            if (self.permission_prompt.is_none() && self.question_prompt.is_none())
                && let Some(ref mut rx) = self.handoff_rx
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

    fn drain_tui_diagnostics(&mut self) -> io::Result<()> {
        // A trace burst cannot monopolize input or agent progress. The next
        // event-loop iteration drains the remaining records.
        if let Some(receiver) = self.tui_log_rx.as_mut() {
            for _ in 0..16 {
                let Ok(message) = receiver.try_recv() else {
                    break;
                };
                for line in message.lines() {
                    self.renderer.write_line(&sanitize_output(line), C_ERROR)?;
                }
            }
        }
        let dropped = crate::logging::take_dropped_tui_diagnostics();
        if dropped > 0 {
            self.renderer.write_line(
                &format!("warning: {dropped} additional diagnostic records omitted"),
                C_ERROR,
            )?;
        }
        Ok(())
    }

    pub(crate) async fn teardown(mut self) {
        // The owning actor must retire before its manager/services are closed,
        // including callers that leave without entering the normal run wrapper.
        #[cfg(feature = "mcp")]
        if let Err(error) = self.retire_mcp_work().await {
            tracing::warn!(%error, "TUI MCP operation cleanup failed");
        }
        if let Err(error) = self.retire_session_title().await {
            tracing::warn!(%error,"session title cleanup failed");
        }
        self.cancel_pending_permissions();
        self.pause_event_thread();

        // Retire owned work before closing the services it can still use.
        const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);
        self.btw_pricing.cancel_all();
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
        #[cfg(feature = "git-worktree")]
        if let Err(error) = self.settle_git_usage() {
            tracing::warn!(%error,"Git usage persistence during teardown failed");
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

    /// Hand the model picker the current provider's cached ids when a
    /// background listing has landed since the last sync (or when `force`).
    fn sync_live_model_names(&mut self, force: bool) {
        let generation = crate::ui::slash::model_cache_generation();
        self.input
            .set_current_model(Some(self.ui.session.model.to_string()));
        if force || generation != self.model_cache_generation {
            self.model_cache_generation = generation;
            self.input
                .set_live_model_names(crate::ui::slash::cached_model_ids(
                    &self.ui.session.provider,
                    self.ui
                        .cfg
                        .custom_providers_map()
                        .contains_key(self.ui.session.provider.as_str()),
                    &self.ui.client,
                ));
        }
    }

    fn refresh(&mut self) -> io::Result<()> {
        self.input.set_file_workspace(self.ui.workspace.clone());
        self.sync_live_model_names(false);
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

    fn begin_user_request(
        &mut self,
        request: crate::permission::ask::UserRequest,
    ) -> anyhow::Result<()> {
        match request {
            #[cfg(feature = "mcp")]
            crate::permission::ask::UserRequest::McpPermission(request) => {
                if !self.mcp_frontend_owner_current(&request.origin.owner)
                    || !(request.origin.valid)()
                    || request.request.reply.is_closed()
                {
                    let _ = request
                        .request
                        .reply
                        .send(crate::permission::ask::UserDecision::Deny);
                    return Ok(());
                }
                let mut prompt = PermissionState::begin(
                    request.request,
                    &mut self.renderer,
                    &self.ui,
                    &mut self.run,
                )?;
                prompt.bind_origin(request.origin.valid);
                prompt.bind_owner(request.origin.owner);
                self.permission_prompt = Some(prompt);
                Ok(())
            }
            #[cfg(feature = "mcp")]
            crate::permission::ask::UserRequest::McpElicitation(request) => {
                let current = (request.valid)()
                    && match &request.owner {
                        crate::extras::mcp::elicitation::FrontendOwner::Main(id) => {
                            self.run.is_running
                                && self
                                    .run
                                    .question_turn
                                    .as_ref()
                                    .is_some_and(|turn| turn.id() == *id)
                        }
                        crate::extras::mcp::elicitation::FrontendOwner::Operator {
                            session,
                            workspace,
                            generation,
                        } => {
                            *session == self.ui.session.id
                                && Arc::ptr_eq(workspace, &self.ui.workspace)
                                && self
                                    .mcp_admin
                                    .as_ref()
                                    .and_then(|state| state.work.as_ref())
                                    .is_some_and(|work| {
                                        work.id == *generation
                                            && work.valid(&self.ui.session.id, &self.ui.workspace)
                                    })
                        }
                    };
                if !current || request.question.reply.is_closed() {
                    let _ = request
                        .question
                        .reply
                        .send(crate::question::QuestionAnswer::Cancelled);
                    return Ok(());
                }
                self.question_prompt =
                    Some(QuestionState::begin(request.question, &mut self.renderer)?);
                self.question_prompt.as_mut().unwrap().bind_origin(
                    crate::extras::mcp::elicitation::AskOrigin {
                        owner: request.owner,
                        valid: request.valid,
                    },
                );
                #[cfg(feature = "goal")]
                if let Some(collector) = self.run.goal_round.as_mut() {
                    collector.pause_clock();
                }
                Ok(())
            }
            crate::permission::ask::UserRequest::Permission(request) => {
                self.begin_permission_request(request)
            }
            crate::permission::ask::UserRequest::Question(request) => {
                if !self.run.is_running
                    || request.reply.is_closed()
                    || self
                        .run
                        .question_turn
                        .as_ref()
                        .is_none_or(|turn| turn.id() != request.id)
                {
                    let _ = request
                        .reply
                        .send(crate::question::QuestionAnswer::Cancelled);
                    return Ok(());
                }
                self.question_prompt = Some(QuestionState::begin(request, &mut self.renderer)?);
                #[cfg(feature = "goal")]
                if let Some(collector) = self.run.goal_round.as_mut() {
                    collector.pause_clock();
                }
                Ok(())
            }
        }
    }

    fn begin_permission_request(
        &mut self,
        request: crate::permission::ask::AskRequest,
    ) -> anyhow::Result<()> {
        // Cancellation settles the owning run before another request can be
        // adopted. A dropped caller or a request left by that run cannot grant.
        if !self.run.is_running || request.reply.is_closed() {
            let _ = request
                .reply
                .send(crate::permission::ask::UserDecision::Deny);
            return Ok(());
        }
        self.permission_prompt = Some(PermissionState::begin(
            request,
            &mut self.renderer,
            &self.ui,
            &mut self.run,
        )?);
        #[cfg(feature = "goal")]
        if let Some(collector) = self.run.goal_round.as_mut() {
            collector.pause_clock();
        }
        Ok(())
    }

    fn resume_permission_clock(&mut self) {
        #[cfg(feature = "goal")]
        if let Some(collector) = self.run.goal_round.as_mut() {
            collector.resume_clock();
        }
    }

    fn expire_abandoned_permission(&mut self) -> anyhow::Result<()> {
        #[cfg(feature = "mcp")]
        let stale_mcp_question = self
            .question_prompt
            .as_ref()
            .and_then(QuestionState::origin_owner)
            .is_some_and(|owner| !self.mcp_frontend_owner_current(owner));
        #[cfg(not(feature = "mcp"))]
        let stale_mcp_question = false;
        if stale_mcp_question
            || self
                .question_prompt
                .as_ref()
                .is_some_and(QuestionState::abandoned)
        {
            if let Some(prompt) = self.question_prompt.take() {
                prompt.finish(&mut self.renderer)?;
            }
            self.resume_permission_clock();
            self.refresh()?;
        }
        #[cfg(feature = "mcp")]
        let stale_mcp_permission = self
            .permission_prompt
            .as_ref()
            .and_then(PermissionState::origin_owner)
            .is_some_and(|owner| !self.mcp_frontend_owner_current(owner));
        #[cfg(not(feature = "mcp"))]
        let stale_mcp_permission = false;
        if stale_mcp_permission
            || self
                .permission_prompt
                .as_ref()
                .is_some_and(PermissionState::abandoned)
        {
            if let Some(prompt) = self.permission_prompt.take() {
                prompt.decline_on_cancel(&self.ui);
                prompt.finish(&mut self.renderer)?;
            }
            self.resume_permission_clock();
            self.refresh()?;
        }
        Ok(())
    }

    /// Deny both the displayed request and the sequential channel backlog
    /// before settlement; no waiter from the retired run can become a grant.
    fn cancel_pending_permissions(&mut self) {
        if let Some(prompt) = self.question_prompt.take() {
            if let Err(error) = prompt.finish(&mut self.renderer) {
                tracing::warn!(%error,"question overlay cleanup failed");
            }
            drop(prompt);
        }
        if let Some(prompt) = self.permission_prompt.take() {
            prompt.decline_on_cancel(&self.ui);
            if let Err(error) = prompt.finish(&mut self.renderer) {
                tracing::warn!(%error, "permission overlay cleanup failed");
            }
            drop(prompt);
        }
        self.renderer.permission_prompt = None;
        if let Some(receiver) = self.ask_rx.as_mut() {
            while let Ok(request) = receiver.try_recv() {
                request.cancel();
            }
        }
        self.resume_permission_clock();
    }

    async fn handle_permission_event(&mut self, event: &UserEvent) -> anyhow::Result<bool> {
        self.expire_abandoned_permission()?;
        if let Some(mut prompt) = self.question_prompt.take() {
            match prompt.handle(event, &mut self.renderer)? {
                QuestionEvent::Unhandled => {
                    self.question_prompt = Some(prompt);
                    return Ok(false);
                }
                QuestionEvent::Waiting => self.question_prompt = Some(prompt),
                QuestionEvent::Finished => self.resume_permission_clock(),
                QuestionEvent::Stop => {
                    drop(prompt);
                    #[cfg(feature = "mcp")]
                    self.retire_mcp_work().await?;
                    self.abort_main_run().await?;
                    if self.run.agent_rx.is_some() {
                        return Err(anyhow::anyhow!(
                            "question cancellation cleanup is still pending; no next turn was admitted"
                        ));
                    }
                    self.renderer.write_line(
                        "question cancelled; run stopped without an answer or permission grant",
                        C_PERM,
                    )?;
                }
            }
            self.refresh()?;
            return Ok(true);
        }
        let Some(mut prompt) = self.permission_prompt.take() else {
            return Ok(false);
        };
        let effect = prompt.handle_event(
            event,
            &mut self.renderer,
            &mut self.ui,
            &mut self.run,
            &mut self.deferred_user_events,
        )?;
        match effect {
            PermissionEvent::Unhandled => {
                self.permission_prompt = Some(prompt);
                return Ok(false);
            }
            PermissionEvent::Waiting => self.permission_prompt = Some(prompt),
            PermissionEvent::Finished => self.resume_permission_clock(),
            PermissionEvent::Stop { instruction } => {
                #[cfg(feature = "mcp")]
                self.retire_mcp_work().await?;
                // Reply and abort happen in one polling turn before awaiting
                // cleanup, preventing a denial from launching an alternative.
                drop(prompt);
                self.abort_main_run().await?;
                if self.run.agent_rx.is_some() {
                    // Ordinary interruption retains cleanup ownership on a
                    // timeout. Permission stop must exit the input loop rather
                    // than admit an instruction or another turn before that
                    // exact workspace has settled; teardown retries it.
                    return Err(anyhow::anyhow!(
                        "permission stop cleanup is still pending; no next turn was admitted"
                    ));
                }
                self.renderer
                    .write_line("permission denied; run stopped", C_PERM)?;
                if instruction {
                    self.permission_prompt =
                        Some(PermissionState::instruction(&mut self.renderer)?);
                }
            }
            PermissionEvent::Instruction(text) => {
                if !text.is_empty() {
                    // Preserve the restored original prompt/follow-up in the
                    // kill ring. This instruction is an editable next draft,
                    // never an automatically admitted queued request.
                    self.input.discard_draft();
                    self.input.load_text(&text);
                    self.renderer.write_line(
                        "stop instruction placed in editor; press Enter to send",
                        C_PERM,
                    )?;
                }
            }
        }
        self.refresh()?;
        Ok(true)
    }

    async fn handle_user_event(&mut self, ev: UserEvent) -> anyhow::Result<ControlFlow<(), ()>> {
        self.input.set_file_workspace(self.ui.workspace.clone());
        #[cfg(feature = "mcp")]
        if crate::startup::heap_future(|| self.handle_mcp_user_event(&ev)).await? {
            return Ok(ControlFlow::Continue(()));
        }
        if crate::startup::heap_future(|| self.handle_plan_event(&ev)).await? {
            return Ok(ControlFlow::Continue(()));
        }
        #[cfg(feature = "git-worktree")]
        if self.chain.git.pending.is_some() {
            if matches!(&ev,UserEvent::Key(key) if matches!(key.code,KeyCode::Char('c'|'d'))&&key.modifiers.contains(KeyModifiers::CONTROL))
            {
                self.chain.git.pending = None;
                self.chain.git.editing = false;
                self.renderer.chain_prompt = None;
                self.renderer.chain_but_mode = false;
                self.discard_interrupted_queue();
                self.renderer
                    .write_line("Git proposal cancelled; no command executed", C_AGENT)?;
                self.refresh()?;
                return Ok(ControlFlow::Continue(()));
            }
            match &ev {
                UserEvent::Key(key)
                    if key.code == KeyCode::BackTab
                        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) => {}
                UserEvent::Key(key) => {
                    crate::startup::heap_future(|| self.handle_git_confirmation(*key)).await?;
                    self.refresh()?;
                    return Ok(ControlFlow::Continue(()));
                }
                UserEvent::Paste(text) => {
                    if self.chain.git.editing {
                        self.input.load_text(text);
                        self.refresh()?;
                    }
                    return Ok(ControlFlow::Continue(()));
                }
                UserEvent::Quit => {
                    self.chain.git.pending = None;
                    self.chain.git.editing = false;
                    self.renderer.chain_prompt = None;
                    self.renderer.chain_but_mode = false;
                }
                _ => {}
            }
        }
        if self.handle_session_picker_event(&ev).await? {
            return Ok(ControlFlow::Continue(()));
        }
        // Keep the measured large legacy handler out of the new picker poll
        // and construct it in a short frame without changing event semantics.
        crate::startup::heap_future(|| self.handle_standard_user_event(ev)).await
    }

    async fn handle_standard_user_event(
        &mut self,
        ev: UserEvent,
    ) -> anyhow::Result<ControlFlow<(), ()>> {
        // A pending approval is never answered by mode cycling. Pickers keep
        // their own BackTab navigation, and modified shortcuts remain ignored.
        if matches!(&ev, UserEvent::Key(key) if key.code == KeyCode::BackTab
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT))
            && !self
                .input
                .picker
                .as_ref()
                .is_some_and(|picker| picker.active())
        {
            self.cycle_permission_mode()?;
            self.refresh()?;
            return Ok(ControlFlow::Continue(()));
        }
        if self.handle_permission_event(&ev).await? {
            return Ok(ControlFlow::Continue(()));
        }
        match ev {
            #[cfg(feature = "mcp")]
            UserEvent::McpAdminDone { .. } => {}
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
                    if self.renderer.toggle_reasoning_at_line(idx) {
                        self.renderer.clear_selection();
                    } else if let Some(url) = self.renderer.link_url_at(idx, col) {
                        let tx = self.user_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = renderer_mod::open_url(&url).await {
                                let _ = tx.send(UserEvent::LinkOpenFailed(error.to_string())).await;
                            }
                        });
                    } else {
                        let text_col = self.renderer.chat_text_col(col);
                        let double = self.renderer.register_click(row, col);
                        if !(double && self.renderer.select_word_at(idx, text_col)) {
                            let point = SelectionPoint::new(idx, text_col);
                            self.renderer.selection_active = true;
                            self.renderer.selection_start = Some(point);
                            self.renderer.selection_end = Some(point);
                            self.renderer.selection_dragged = false;
                            self.renderer.selection_word = false;
                        }
                    }
                }
            }
            UserEvent::MouseDrag { row, col } => {
                if self.renderer.selection_active {
                    // Dragging after a double-click extends from the word.
                    self.renderer.selection_word = false;
                    self.renderer.selection_dragged = true;
                    if let Some(idx) = self.renderer.buffer_line_at_row(row) {
                        self.renderer.selection_end =
                            Some(SelectionPoint::new(idx, self.renderer.chat_text_col(col)));
                    }
                }
            }
            UserEvent::MouseUp { row, col } => {
                if self.renderer.selection_active {
                    if !self.renderer.selection_word
                        && let Some(idx) = self.renderer.buffer_line_at_row(row)
                    {
                        self.renderer.selection_end =
                            Some(SelectionPoint::new(idx, self.renderer.chat_text_col(col)));
                    }
                    if mouse_up_copies(
                        self.renderer.selection_dragged || self.renderer.selection_word,
                        self.renderer.selection_start,
                        self.renderer.selection_end,
                    ) {
                        self.copy_selection_to_clipboard().await?;
                    } else {
                        // A plain click: drop the one-line highlight silently.
                        self.renderer.clear_selection();
                    }
                }
            }
            UserEvent::Quit => {
                self.cancel_pending_permissions();
                // The terminal may already be gone (SIGHUP): drawing can fail,
                // but stopping the run and saving must still happen.
                if self.run.is_running
                    && let Err(error) = self.abort_main_run().await
                {
                    tracing::warn!(%error, "run cleanup on quit failed");
                }
                let _ = self.save_session();
                return Ok(ControlFlow::Break(()));
            }
            UserEvent::LinkOpenFailed(error) => {
                self.renderer
                    .write_line(&format!("cannot open link: {error}"), C_ERROR)?;
            }
            UserEvent::Paste(data) => {
                self.input.handle_paste(data);
            }
            #[cfg(feature = "goal")]
            UserEvent::GoalVerificationDone(event) => {
                self.handle_goal_verification_event(event).await?;
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
                    let validation_active = self.run.validation_active();
                    match interrupt_target(
                        self.btw_inflight,
                        validation_active,
                        self.run.is_running,
                    ) {
                        InterruptTarget::Btw => {
                            self.btw_pricing.cancel_all();
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
                        InterruptTarget::Exit => {
                            match idle_interrupt(is_ctrl_d, self.input.buffer.is_empty()) {
                                IdleInterrupt::Exit => return Ok(ControlFlow::Break(())),
                                IdleInterrupt::ClearDraft => self.input.discard_draft(),
                                IdleInterrupt::EditorKey => {
                                    crate::startup::heap_future(|| self.handle_key_event(key))
                                        .await?;
                                }
                            }
                        }
                    }
                    self.refresh()?;
                    return Ok(ControlFlow::Continue(()));
                }

                // The measured key-handler future makes this generic event
                // poll too large for the native 1 MiB stack. Construct it in
                // the existing short factory, retaining the same owned await.
                if let Err(e) = crate::startup::heap_future(|| self.handle_key_event(key)).await {
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
            // The outcome is a transient status-line notice: copying must not
            // grow the transcript it is copying from.
            Ok(ClipboardCopyOutcome::Confirmed) => {
                self.renderer.show_notice("copied selection", Color::Green);
                self.renderer.clear_selection();
            }
            Ok(ClipboardCopyOutcome::FallbackRequested) => {
                self.renderer
                    .show_notice("copy requested through terminal", Color::Green);
                self.renderer.clear_selection();
            }
            Err(error) => {
                self.renderer
                    .show_notice(&format!("copy to clipboard failed: {error}"), C_ERROR);
                self.renderer.clear_selection();
            }
        }
        Ok(())
    }

    fn cycle_permission_mode(&mut self) -> anyhow::Result<()> {
        let Some(permission) = &self.ui.permission else {
            self.renderer
                .show_notice("permission system not active", C_ERROR);
            return Ok(());
        };
        let ring =
            crate::permission::permission_mode_cycle(self.ui.cfg.permission_mode_cycle.as_deref())
                .map_err(anyhow::Error::msg)?;
        let mut checker = permission.lock().unwrap_or_else(|error| error.into_inner());
        let index = ring.iter().position(|mode| *mode == checker.mode());
        let mode = ring[index.map_or(0, |index| (index + 1) % ring.len())];
        checker.set_mode(mode);
        drop(checker);
        self.renderer
            .show_notice(&format!("security mode: {mode}"), Color::Green);
        Ok(())
    }

    async fn handle_session_picker_event(&mut self, event: &UserEvent) -> anyhow::Result<bool> {
        let handled = match self.input.picker.as_mut() {
            Some(crate::ui::input::Picker::Sessions(picker)) if picker.active => match event {
                UserEvent::Key(key) => picker.handle(*key),
                UserEvent::Paste(text) => {
                    picker.paste(text);
                    true
                }
                _ => false,
            },
            _ => false,
        };
        if !handled {
            return Ok(false);
        }
        if let Some(outcome) = self.input.take_session_outcome() {
            self.input.picker = None;
            self.renderer.invalidate();
            match outcome {
                crate::ui::pickers::sessions::SessionPick::Reload(all) => {
                    if self.run.is_running || self.run.pending_turn.is_some() {
                        self.input.picker = None;
                        self.renderer.write_line(
                            "finish or stop the current turn before reopening a session selector",
                            C_ERROR,
                        )?;
                        self.refresh()?;
                        return Ok(true);
                    }
                    let catalogue = crate::session::catalogue::list(&self.ui.workspace, all)?;
                    self.input
                        .start_session_picker(catalogue, self.ui.workspace.clone(), all);
                }
                crate::ui::pickers::sessions::SessionPick::Cancelled => {}
                crate::ui::pickers::sessions::SessionPick::Selected(selection) => {
                    if self.run.is_running
                        || self.run.pending_turn.is_some()
                        || self.permission_prompt.is_some()
                        || self.question_prompt.is_some()
                    {
                        self.renderer.write_line(
                            "finish or stop the current turn before resuming a session",
                            C_ERROR,
                        )?;
                        self.refresh()?;
                        return Ok(true);
                    }
                    let selected = (|| -> anyhow::Result<_> {
                        anyhow::ensure!(
                            Arc::ptr_eq(&selection.workspace, &self.ui.workspace),
                            "workspace changed; reopen the session picker"
                        );
                        crate::session::catalogue::selected(
                            &selection.row,
                            &self.ui.workspace,
                            selection.all,
                        )
                    })();
                    match selected {
                        Ok(session) => {
                            self.retire_session_title().await?;
                            let command = format!("/sessions {}", selection.row.id);
                            // Construct the selected-session slash future in
                            // the short factory frame, not this picker poll.
                            crate::startup::heap_future(|| {
                                self.run_slash_command_selected(&command, Some(Box::new(session)))
                            })
                            .await?;
                        }
                        Err(error) => self.renderer.write_line(
                            &format!(
                                "session selection failed: {}",
                                sanitize_output(&error.to_string())
                            ),
                            C_ERROR,
                        )?,
                    }
                }
            }
        }
        self.refresh()?;
        Ok(true)
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
            let current = self.renderer.feed().reasoning_display();
            let display = current.next();
            let mut before = self.ui.cfg.clone();
            // The startup config is immutable. Compare this key with its
            // effective live value so repeated cycles persist every transition.
            before.reasoning_display = Some(current);
            let mut after = before.clone();
            after.reasoning_display = Some(display);
            match crate::config::save_config_changes(&before, &after) {
                Ok(()) => {
                    self.slash.show_reasoning = display != crate::config::ReasoningDisplay::Off;
                    self.renderer.clear_selection();
                    self.renderer.feed_mut().set_reasoning_display(display);
                    self.renderer.show_notice(
                        &format!("reasoning visibility: {}", display.label()),
                        Color::White,
                    );
                    self.refresh()?;
                }
                Err(error) => self.renderer.write_line(
                    &format!("cannot save reasoning preference: {error}"),
                    C_ERROR,
                )?,
            }
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
            _ => {}
        }
        match transcript_scroll_key(
            key,
            self.input.buffer.is_empty(),
            self.input.picker.as_ref().is_some_and(|p| p.active()),
        ) {
            Some(TranscriptScroll::Top) => {
                self.renderer.scroll_to_top();
                return Ok(());
            }
            Some(TranscriptScroll::Bottom) => {
                self.renderer.scroll_to_bottom()?;
                return Ok(());
            }
            None => {}
        }

        if self.input.picker.as_ref().is_some_and(|p| p.active())
            && self.input.handle_picker_key(key)
        {
            if let Some(RewindOutcome::Confirmed(idx)) = self.input.take_rewind_outcome()
                && crate::ui::slash::rewind_selected_turn(self.ui.session, &mut self.input, idx) > 0
            {
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
            return Ok(());
        }

        if key.code == KeyCode::Char('/') && self.input.buffer.is_empty() && !self.run.is_running {
            self.refresh_custom_commands();
        }
        #[cfg(feature = "mcp")]
        if key.code == KeyCode::Char('@') && !self.run.is_running {
            self.refresh_custom_commands();
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

        if is_lazygit_key(key) {
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
                        self.run_chain_transition(
                            phase,
                            None,
                            crate::question::QuestionOrigin::ContinueUserTurn,
                        )
                        .await?;
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
                self.run_chain_transition(
                    phase,
                    Some(&trimmed),
                    crate::question::QuestionOrigin::NewUserTurn,
                )
                .await?;
                return Ok(());
            }

            match classify_submission(self.run.is_running, &text, &self.ui.context.prompts) {
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

            let one_shot = matches!(
                super::dot_submission(&text, &self.ui.context.prompts),
                super::DotSubmission::OneShot { .. }
            );
            if self.handle_dot_command(&mut text).await? {
                return Ok(());
            }

            if !one_shot
                && (text.starts_with('/') || crate::ui::slash::is_readonly_diagnostic(&text))
            {
                self.run_slash_command(&text).await?;
            } else if !one_shot && text.starts_with('!') {
                self.run_bang_command(&text).await?;
            } else {
                #[cfg(feature = "mcp")]
                if crate::startup::heap_future(|| self.start_mcp_mentions(&text)).await? {
                    return Ok(());
                }
                self.echo_main_submission(&text)?;
                self.start_main_run(&text, crate::question::QuestionOrigin::NewUserTurn)
                    .await?;
            }
        }

        Ok(())
    }

    async fn handle_agent_event(&mut self, event: AgentEvent) -> anyhow::Result<()> {
        // Todo tools mutate the shared store before their result is delivered.
        // Chat paints alone do not refresh the sticky numeric footer.
        let todo_progress_changed =
            matches!(&event, AgentEvent::ToolResult { name, .. } if name == "todo_write");
        #[cfg(feature = "git-worktree")]
        if self.chain.git.work.is_some()
            && matches!(&event, AgentEvent::Done { .. } | AgentEvent::Error { .. })
        {
            return crate::startup::heap_future(|| self.handle_git_terminal(event)).await;
        }

        if matches!(
            &event,
            AgentEvent::Done { .. } | AgentEvent::Error { .. } | AgentEvent::AwaitingUser { .. }
        ) {
            self.cancel_pending_permissions();
        }
        #[cfg(feature = "git-worktree")]
        if let Some(work) = self.chain.git.work.as_mut() {
            match &event {
                AgentEvent::ToolCall { id, name, .. } => {
                    work.pending_calls.insert(id.to_string(), name.to_string());
                }
                AgentEvent::ToolResult { id, .. } => {
                    work.pending_calls.remove(id.as_str());
                }
                _ => {}
            }
        }
        let completed_plan = match &event {
            AgentEvent::Done { response, .. } => self
                .chain
                .plan
                .run
                .take()
                .map(|captured| (captured, response.clone())),
            AgentEvent::Error { .. } | AgentEvent::AwaitingUser { .. } => {
                self.chain.plan = Default::default();
                None
            }
            _ => None,
        };
        let terminal_error = matches!(&event, AgentEvent::Error { .. });
        if terminal_error {
            self.discard_interrupted_queue();
        }
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
            AgentEvent::Error { .. } | AgentEvent::AwaitingUser { .. } => {
                self.run.awaiting_compaction_relief = false;
                self.run.pending_compaction_pressure = None;
            }
            _ => {}
        }

        #[cfg(feature = "loop")]
        let loop_running = self.ui.session.goal_store.is_active();
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
            #[cfg(any(feature = "loop", feature = "goal"))]
            &self.user_tx,
        )
        .await;

        if let Err(error) = handled {
            if let Some(text) = failed_prompt {
                self.restore_interrupted_prompt(&text);
            }
            if terminal_success {
                // The completed response/accounting is installed before
                // fallible presentation. Make that valid success durable even
                // when rendering or terminal post-processing fails.
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

        if terminal_success {
            if let Some((captured, response)) = completed_plan
                && !self.plan_execution_busy()
            {
                match captured.complete(
                    &response,
                    self.ui.session,
                    self.ui.context,
                    &self.ui.workspace,
                    &self.ui.permission,
                ) {
                    Ok(proposal) => {
                        self.chain.plan.pending = Some(proposal);
                        self.chain.plan.editing = false;
                        self.chain.pending = None;
                        self.chain.label_msg = None;
                        self.show_plan_approval()?;
                    }
                    Err(message) => self.renderer.write_line(message, C_ERROR)?,
                }
            }
            self.request_session_title();
        }

        if let Some(text) = failed_prompt {
            self.restore_interrupted_prompt(&text);
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
        if todo_progress_changed {
            self.refresh()?;
        }
        Ok(())
    }

    #[cfg(feature = "goal")]
    async fn handle_goal_verification_event(
        &mut self,
        event: Box<crate::event::GoalVerificationEvent>,
    ) -> anyhow::Result<()> {
        let relaunched = event_handler::handle_goal_verification_event(
            event,
            &mut self.renderer,
            &mut self.run,
            &mut self.ui,
        )
        .await?;
        if !relaunched {
            self.finalize_turn().await?;
        }
        Ok(())
    }

    async fn finalize_turn(&mut self) -> anyhow::Result<()> {
        #[cfg(feature = "git-worktree")]
        if self.chain.git.pending.is_some() {
            return Ok(());
        }
        if !self.run.is_running {
            self.settle_success_transaction();
        }
        if self.chain.plan.pending.is_some() || self.ui.session.pending_question.is_some() {
            return Ok(());
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
            && phase != crate::extras::chain::ChainPhase::Plan
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
            if !self.run.pending_inputs.is_empty() && self.budget_admission_notice()? {
                // Keep queued input visible for the owner to remove/resubmit;
                // cap exhaustion must never dequeue or start a request.
                return Ok(());
            }
            if let Some(next) = self.run.pending_inputs.pop_front() {
                #[cfg(feature = "mcp")]
                if crate::startup::heap_future(|| self.start_mcp_mentions(&next)).await? {
                    return Ok(());
                }
                self.renderer.chain_prompt = None;
                self.renderer.chain_but_mode = false;
                self.chain.pending = None;
                self.chain.label_msg = None;
                for line in next.lines() {
                    self.renderer
                        .write_line(&format!("> {}", sanitize_output(line)), Color::Green)?;
                }
                self.renderer.write_line("", Color::White)?;
                self.start_main_run(&next, crate::question::QuestionOrigin::NewUserTurn)
                    .await?;
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
        self.chain.plan = Default::default();
        self.renderer.feed_mut().finish_reasoning();
        self.cancel_pending_permissions();
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
        self.cancel_pending_permissions();
        #[cfg(feature = "git-worktree")]
        if let Err(error) = self.settle_git_usage() {
            tracing::warn!(%error,"Git usage persistence after failed run failed");
        }
        self.run.compaction_decision_tx = None;
        self.run.turn_trace.clear();
        self.run.awaiting_compaction_relief = false;
        self.run.pending_compaction_pressure = None;
        self.discard_interrupted_queue();
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
                self.restore_interrupted_prompt(&text);
            }
            let _ = self.save_session();
        }
    }

    async fn abort_main_run(&mut self) -> anyhow::Result<()> {
        self.chain.plan = Default::default();
        self.renderer.feed_mut().finish_reasoning();
        #[cfg(feature = "git-worktree")]
        {
            self.chain.git.pending = None;
            self.chain.git.editing = false;
            self.renderer.chain_prompt = None;
            self.renderer.chain_but_mode = false;
        }
        self.cancel_pending_permissions();
        let preserve_progress = preserve_pending_main_turn_progress(&mut self.run, self.ui.session);
        let validation_active = self.run.cancel_validation();

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
        self.cancel_pending_permissions();
        #[cfg(feature = "git-worktree")]
        self.settle_git_usage()?;
        self.run.compaction_decision_tx = None;
        self.run.turn_trace.clear();
        self.run.awaiting_compaction_relief = false;
        self.run.pending_compaction_pressure = None;
        self.discard_interrupted_queue();
        let failed_prompt = (!preserve_progress)
            .then(|| rollback_pending_main_turn(&mut self.run, self.ui.session))
            .flatten();
        // An interrupt is not the agent's verdict on anything, so the round it
        // stopped is settled through the gate's own interrupt row: the goal is
        // left exactly as it was, uncounted, ready to resume. Writing a status
        // here instead would park it with no reason to show the user, and
        // would disagree with what the headless and editor surfaces do with
        // the same signal.
        //
        // The round's collector goes with it. A collector left behind is
        // adopted by the next ordinary turn, which then inherits this round's
        // tool counts and a clock that has been running since the interrupt —
        // so a stretch of idle time lands in the goal's time budget.
        #[cfg(feature = "goal")]
        if let Some(collector) = self.run.goal_round.take()
            && let Some(goal) = self.ui.session.goal_store.snapshot()
            && !goal.status.is_terminal()
        {
            let summary =
                collector.finish(&goal, crate::extras::goal::gate::RoundEnd::Cancelled, 0);
            let outcome = crate::extras::goal::driver::apply_decision(
                &self.ui.session.goal_store,
                &summary,
                crate::extras::goal::gate::gate_interrupted(),
                &crate::extras::goal::driver::Verification::default(),
            );
            if let crate::extras::goal::driver::RoundOutcome::Stopped { line, .. } = outcome {
                let _ = self.renderer.write_line(&line, C_AGENT);
            }
            self.chain.loop_label = None;
        }
        if let Some(text) = failed_prompt {
            self.restore_interrupted_prompt(&text);
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

    fn restore_interrupted_prompt(&mut self, text: &str) {
        if !self.input.buffer.is_empty() && self.input.buffer != text {
            self.input.discard_draft();
        }
        self.input.load_text(text);
    }

    /// Queued prompts must not restart the agent after an interrupt. Make the
    /// discard visible; failure cleanup remains safe if presentation also fails.
    fn discard_interrupted_queue(&mut self) {
        let count = self.run.pending_inputs.len();
        self.run.pending_inputs.clear();
        if count > 0
            && let Err(error) = self
                .renderer
                .write_line(&format!("dropped {count} queued inputs"), C_TOOL)
        {
            tracing::warn!(%error, count, "interrupted-run queue discard could not be displayed");
        }
    }

    #[cfg(feature = "git-worktree")]
    fn show_git_proposal(
        &mut self,
        proposal: crate::ui::slash::git::BoundProposal,
    ) -> anyhow::Result<()> {
        use crate::ui::slash::git::PreparedAction;
        anyhow::ensure!(
            proposal.valid(self.ui.session, &self.ui.workspace),
            "Git proposal is stale; run the command again"
        );
        match &proposal.action {
            PreparedAction::Commit { paths, message } => {
                self.renderer.write_line(
                    "Files requested by write/edit tools this turn (a request may have failed):",
                    C_TOOL,
                )?;
                for path in paths {
                    self.renderer
                        .write_line(&format!("  {}", sanitize_output(path)), C_TOOL)?;
                }
                self.renderer.write_line(
                    &format!("Commit message:\n{}", sanitize_output(message)),
                    C_AGENT,
                )?;
            }
            PreparedAction::PullRequest { title, body } => self.renderer.write_line(
                &format!(
                    "PR title: {}\nPR body:\n{}",
                    sanitize_output(title),
                    sanitize_output(body)
                ),
                C_AGENT,
            )?,
        }
        self.renderer.chain_but_mode = false;
        self.renderer.chain_prompt = Some(ChainPrompt {
            question: proposal.action.label().into(),
        });
        self.chain.git.pending = Some(proposal);
        self.refresh()?;
        Ok(())
    }

    #[cfg(feature = "git-worktree")]
    fn start_git_work(
        &mut self,
        action: crate::ui::slash::git::NativeAction,
        kind: crate::ui::slash::git::WorkKind,
        usage: crate::ui::slash::git::UsageLedger,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.run.is_running
                && self.run.agent_rx.is_none()
                && self.run.pending_turn.is_none()
                && (self.permission_prompt.is_none() && self.question_prompt.is_none()),
            "finish or stop the current run before a Git command"
        );
        self.ui.workspace.validate().map_err(anyhow::Error::msg)?;
        let (runner, observations) = crate::ui::slash::git::spawn_native(
            action,
            self.ui.workspace.clone(),
            self.ui.sandbox.clone(),
            self.ui.permission.clone(),
            self.ui.ask_tx.clone(),
            self.ui.cfg.resolve_shell_timeout_secs(),
        );
        self.chain.git.work = Some(crate::ui::slash::git::Work {
            session_id: self.ui.session.id.clone(),
            workspace: self.ui.workspace.clone(),
            kind,
            observations,
            usage,
            pricing: crate::print::HeadlessPricing::from_session(self.ui.session, self.ui.cfg),
            provider: self.ui.session.provider.clone(),
            pending_calls: Default::default(),
        });
        self.run.agent_rx = Some(runner.event_rx);
        self.run.main_abort = Some(runner.abort_handle);
        self.run.is_running = true;
        self.renderer.chain_prompt = None;
        self.renderer.chain_but_mode = false;
        self.chain.git.editing = false;
        self.refresh()?;
        Ok(())
    }

    #[cfg(feature = "git-worktree")]
    async fn begin_git_command(&mut self, text: &str) -> anyhow::Result<()> {
        use crate::ui::slash::git::{
            self, Command, NativeAction, PreparedAction, ProposalKind, WorkKind,
        };
        anyhow::ensure!(
            !self.run.is_running
                && self.run.agent_rx.is_none()
                && self.run.pending_turn.is_none()
                && (self.permission_prompt.is_none() && self.question_prompt.is_none()),
            "finish or stop the current run before a Git command"
        );
        self.chain.pending = None;
        self.chain.label_msg = None;
        self.renderer.chain_prompt = None;
        self.renderer.chain_but_mode = false;
        self.chain.git.pending = None;
        let command = git::parse(text, self.ui.session, self.ui.workspace.root())
            .map_err(anyhow::Error::msg)?;
        let usage = git::UsageLedger::default();
        let kind = match command {
            Command::Diff(paths) => {
                return self.start_git_work(NativeAction::Diff { paths }, WorkKind::Diff, usage);
            }
            Command::Commit(message) => {
                let paths = git::latest_turn_paths(self.ui.session, self.ui.workspace.root())
                    .map_err(anyhow::Error::msg)?;
                if let Some(message) = message {
                    let action =
                        PreparedAction::commit(paths, message).map_err(anyhow::Error::msg)?;
                    return self.show_git_proposal(git::BoundProposal {
                        session_id: self.ui.session.id.clone(),
                        workspace: self.ui.workspace.clone(),
                        action,
                    });
                }
                anyhow::ensure!(
                    !paths.is_empty(),
                    "latest turn has no native edit/write request paths"
                );
                ProposalKind::Commit(paths)
            }
            Command::PullRequest => ProposalKind::PullRequest,
        };
        let spend = crate::spend::run::Run::for_session(
            self.ui.session,
            self.ui.cfg,
            crate::spend::RequestKind::GitProposal,
        );
        spend.prepare_owned().await?;
        let usage = git::UsageLedger::with_spend(spend);
        self.renderer.write_line(
            "Drafting a Git proposal; review and confirm before any commit or PR runs.",
            C_TOOL,
        )?;
        self.start_git_work(
            NativeAction::Prepare(git::Generation {
                kind: kind.clone(),
                client: self.ui.client.clone(),
                model: self.ui.session.model.to_string(),
                config: Arc::new(self.ui.cfg.clone()),
                session_id: self.ui.session.id.to_string(),
                usage: usage.clone(),
            }),
            WorkKind::Generate(kind),
            usage,
        )
    }

    #[cfg(feature = "git-worktree")]
    fn drain_git_observations(&mut self) -> anyhow::Result<()> {
        let Some(work) = self.chain.git.work.as_mut() else {
            return Ok(());
        };
        anyhow::ensure!(
            work.session_id == self.ui.session.id
                && Arc::ptr_eq(&work.workspace, &self.ui.workspace),
            "Git work identity changed before settlement"
        );
        while let Ok(observation) = work.observations.try_recv() {
            let limit = self.ui.cfg.mutation_diff_options().max_lines;
            let mut count = 0usize;
            for line in observation.text.lines().take(limit) {
                let style = if line.starts_with("@@") {
                    crate::ui::feed::BlockStyle::DiffHunk
                } else if line.starts_with('+') && !line.starts_with("+++") {
                    crate::ui::feed::BlockStyle::DiffAdd
                } else if line.starts_with('-') && !line.starts_with("---") {
                    crate::ui::feed::BlockStyle::DiffDel
                } else {
                    crate::ui::feed::BlockStyle::ToolResult
                };
                self.renderer.feed_mut().place_after_anchor(
                    &observation.id,
                    style,
                    sanitize_output(line),
                );
                count += 1;
            }
            if observation.truncated || observation.text.lines().count() > count {
                self.renderer.feed_mut().place_after_anchor(
                    &observation.id,
                    crate::ui::feed::BlockStyle::ToolResult,
                    "[more Git diff rows omitted]",
                );
            }
        }
        self.renderer.invalidate();
        self.renderer.render_viewport()?;
        Ok(())
    }

    #[cfg(feature = "git-worktree")]
    fn settle_git_usage(&mut self) -> anyhow::Result<Option<crate::ui::slash::git::Work>> {
        if self.run.agent_rx.is_some() {
            return Ok(None);
        }
        let Some(work) = self.chain.git.work.take() else {
            return Ok(None);
        };
        anyhow::ensure!(
            work.session_id == self.ui.session.id
                && Arc::ptr_eq(&work.workspace, &self.ui.workspace),
            "Git work identity changed before settlement"
        );
        let usage = self
            .ui
            .cfg
            .normalize_usage(&work.provider, work.usage.take());
        self.ui.session.charge_usage_delta_at(
            usage,
            work.pricing.anthropic_native,
            work.pricing.input_token_cost,
            work.pricing.output_token_cost,
            work.pricing.cache_prices,
        );
        for (id, name) in &work.pending_calls {
            self.ui.session.add_tool_result_with_id(id,name,"Native command interrupted before its result was observed; effects may be partial. Inspect the workspace before retrying.");
        }
        self.save_session()?;
        Ok(Some(work))
    }

    #[cfg(feature = "git-worktree")]
    async fn handle_git_terminal(&mut self, event: AgentEvent) -> anyhow::Result<()> {
        use crate::ui::slash::git::{BoundProposal, PreparedAction, ProposalKind, WorkKind};
        self.drain_git_observations()?;
        self.run.is_running = false;
        self.run.settle(Duration::from_secs(5)).await?;
        self.run.main_abort = None;
        let work = self
            .settle_git_usage()?
            .ok_or_else(|| anyhow::anyhow!("Git work failed to settle"))?;
        match event {
            AgentEvent::Done { response, .. } => match work.kind {
                WorkKind::Generate(kind) => {
                    let action = match kind {
                        ProposalKind::Commit(paths) => {
                            PreparedAction::generated_commit(paths, &response)
                        }
                        ProposalKind::PullRequest => PreparedAction::generated_pr(&response),
                    };
                    match action {
                        Ok(action) => self.show_git_proposal(BoundProposal {
                            session_id: work.session_id,
                            workspace: work.workspace,
                            action,
                        })?,
                        Err(error) => self.renderer.write_line(
                            &format!("Git proposal rejected: {}", sanitize_output(&error)),
                            C_ERROR,
                        )?,
                    }
                }
                WorkKind::Execute => {
                    self.renderer
                        .write_line(&sanitize_output(&response), C_AGENT)?;
                    if !response.is_empty() {
                        self.ui
                            .session
                            .add_message(crate::session::MessageRole::Assistant, &response);
                    }
                    self.save_session()?;
                }
                WorkKind::Diff => {}
            },
            AgentEvent::Error { message, .. } => {
                self.chain.git.pending = None;
                self.discard_interrupted_queue();
                self.renderer.write_line(
                    &format!("Git command failed: {}", sanitize_output(&message)),
                    C_ERROR,
                )?;
            }
            _ => unreachable!("only terminal Git events are routed here"),
        }
        if self.chain.git.pending.is_none() {
            self.advance_git_queue().await?;
        }
        self.refresh()?;
        Ok(())
    }

    #[cfg(feature = "git-worktree")]
    async fn advance_git_queue(&mut self) -> anyhow::Result<()> {
        if let Some(next) = self.run.pending_inputs.pop_front() {
            #[cfg(feature = "mcp")]
            if crate::startup::heap_future(|| self.start_mcp_mentions(&next)).await? {
                return Ok(());
            }
            self.echo_main_submission(&next)?;
            self.start_main_run(&next, crate::question::QuestionOrigin::NewUserTurn)
                .await?;
        }
        Ok(())
    }

    #[cfg(feature = "git-worktree")]
    async fn handle_git_confirmation(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        use crate::ui::slash::git::{NativeAction, UsageLedger, WorkKind};
        if self.chain.git.pending.is_none() {
            return Ok(false);
        }
        if self.chain.git.editing {
            if key.code == KeyCode::Esc {
                self.input.discard_draft();
                self.chain.git.editing = false;
                let proposal = self.chain.git.pending.take().unwrap();
                self.show_git_proposal(proposal)?;
                return Ok(true);
            }
            if let Some(note) = self.input.handle_key(key) {
                let mut proposal = self.chain.git.pending.take().unwrap();
                if !note.trim().is_empty() {
                    match proposal.action.clone().with_note(&note) {
                        Ok(action) => proposal.action = action,
                        Err(error) => {
                            self.chain.git.pending = Some(proposal);
                            self.input.load_text(&note);
                            self.renderer.write_line(
                                &format!("Proposal text rejected: {}", sanitize_output(&error)),
                                C_ERROR,
                            )?;
                            return Ok(true);
                        }
                    }
                }
                self.chain.git.editing = false;
                self.show_git_proposal(proposal)?;
            }
            return Ok(true);
        }
        if key.kind != crossterm::event::KeyEventKind::Press
            || !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
        {
            return Ok(true);
        }
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                let proposal = self.chain.git.pending.take().unwrap();
                self.renderer.chain_prompt = None;
                if !proposal.valid(self.ui.session, &self.ui.workspace) {
                    self.renderer
                        .write_line("Git proposal is stale; no command executed", C_ERROR)?;
                    return Ok(true);
                }
                self.start_git_work(
                    NativeAction::Execute(proposal.action),
                    WorkKind::Execute,
                    UsageLedger::default(),
                )?;
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.chain.git.pending = None;
                self.renderer.chain_prompt = None;
                self.renderer.chain_but_mode = false;
                self.renderer
                    .write_line("Git proposal declined; no command executed", C_AGENT)?;
                self.advance_git_queue().await?;
            }
            KeyCode::Char('b' | 'B') => {
                self.chain.git.editing = true;
                self.renderer.chain_prompt = None;
                self.renderer.chain_but_mode = true;
                self.input.discard_draft();
                self.renderer.write_line("Edit commit message, or append a PR note; Enter previews again, Esc goes back.",C_AGENT)?;
            }
            _ => {}
        }
        Ok(true)
    }

    fn plan_execution_busy(&self) -> bool {
        if self.run.is_running
            || self.run.agent_rx.is_some()
            || self.run.main_abort.is_some()
            || self.permission_prompt.is_some()
            || self.run.validation_active()
        {
            return true;
        }
        #[cfg(feature = "git-worktree")]
        if self.chain.git.work.is_some() || self.chain.git.pending.is_some() {
            return true;
        }
        #[cfg(feature = "goal")]
        if self
            .ui
            .session
            .goal_store
            .snapshot()
            .is_some_and(|goal| !goal.status.is_terminal())
        {
            return true;
        }
        false
    }

    fn show_plan_approval(&mut self) -> anyhow::Result<()> {
        self.renderer.chain_but_mode = false;
        self.renderer.chain_prompt = Some(ChainPrompt {
            question: compact_str::CompactString::from(crate::ui::plan::QUESTION),
        });
        self.renderer
            .write_line(crate::ui::plan::QUESTION, C_AGENT)?;
        self.refresh()?;
        Ok(())
    }

    async fn handle_plan_event(&mut self, event: &UserEvent) -> anyhow::Result<bool> {
        if self.chain.plan.pending.is_none() {
            return Ok(false);
        }
        if matches!(event, UserEvent::Quit) {
            self.chain.plan = Default::default();
            self.renderer.chain_prompt = None;
            self.renderer.chain_but_mode = false;
            return Ok(false);
        }
        let key = match event {
            UserEvent::Paste(text) => {
                if self.chain.plan.editing {
                    self.input.handle_paste(text.clone());
                    self.refresh()?;
                }
                return Ok(true);
            }
            UserEvent::Key(key) => *key,
            _ => return Ok(false),
        };
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Ok(true);
        }
        // Display-only controls remain available while approval owns input.
        if (key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL))
            || matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
            || (self.renderer.selection_active
                && matches!(key.code, KeyCode::Char('y') | KeyCode::Esc))
        {
            return Ok(false);
        }
        if matches!(key.code, KeyCode::Char('c' | 'd'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            if self.chain.plan.editing {
                self.input.discard_draft();
            }
            self.chain.plan = Default::default();
            self.chain.dot_prompt_restore = None;
            self.renderer.chain_prompt = None;
            self.renderer.chain_but_mode = false;
            self.discard_interrupted_queue();
            self.renderer
                .write_line("Plan approval cancelled; still planning", C_AGENT)?;
            self.refresh()?;
            return Ok(true);
        }
        // Cycling never answers approval; its ceiling change makes the old
        // capture stale, which is checked again before any continuation.
        if key.code == KeyCode::BackTab
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
        {
            return Ok(false);
        }
        if self.chain.plan.editing {
            if key.code == KeyCode::Esc {
                self.input.discard_draft();
                self.chain.plan.editing = false;
                self.show_plan_approval()?;
            } else if let Some(text) = self.input.handle_key(key) {
                if let Err(message) = self.chain.plan.pending.as_mut().unwrap().edit(&text) {
                    self.input.load_text(&text);
                    self.renderer.write_line(message, C_ERROR)?;
                } else {
                    self.chain.plan.editing = false;
                    self.renderer
                        .write_line("Edited plan (awaiting approval):", C_AGENT)?;
                    self.renderer.write_line(&sanitize_output(&text), C_AGENT)?;
                    self.show_plan_approval()?;
                }
            }
            self.refresh()?;
            return Ok(true);
        }
        if !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
            return Ok(true);
        }
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                if self.plan_execution_busy() || self.run.pending_turn.is_some() {
                    self.renderer.write_line(
                        "Finish or stop the active work before approving execution",
                        C_ERROR,
                    )?;
                    return Ok(true);
                }
                let proposal = self.chain.plan.pending.take().unwrap();
                self.renderer.chain_prompt = None;
                self.renderer.chain_but_mode = false;
                self.chain.dot_prompt_restore = None;
                if !proposal.valid(
                    self.ui.session,
                    self.ui.context,
                    &self.ui.workspace,
                    &self.ui.permission,
                ) {
                    self.discard_interrupted_queue();
                    self.renderer
                        .write_line("Plan approval is stale; no execution started", C_ERROR)?;
                    self.refresh()?;
                    return Ok(true);
                }
                let text = proposal.continuation();
                self.discard_interrupted_queue();
                self.input.discard_draft();
                self.ui.invalidate_prebuild();
                if let Some(permission) = &self.ui.permission {
                    permission
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .restore_user_mode();
                }
                apply_prompt_mode("code", self.ui.context, &self.ui.permission);
                apply_prompt_model(
                    "code",
                    &mut self.ui,
                    &mut self.run.agent,
                    self.slash.reasoning_enabled,
                    &mut self.renderer,
                )
                .await;
                self.run.agent = None;
                self.echo_main_submission(&text)?;
                // This is an ordinary user turn: fresh request profile and
                // budget admission, then ordinary per-tool Ask/deny checks.
                let result = crate::startup::heap_future(|| {
                    self.start_main_run(&text, crate::question::QuestionOrigin::NewUserTurn)
                })
                .await;
                if !self.run.is_running {
                    self.input.load_text(&text);
                    if let Err(error) = result {
                        self.renderer.write_line(
                            &format!(
                                "Approved continuation not started: {}",
                                sanitize_output(&error.to_string())
                            ),
                            C_ERROR,
                        )?;
                    } else {
                        self.renderer.write_line(
                            "Approved continuation not started; recoverable text is in the editor",
                            C_ERROR,
                        )?;
                    }
                } else {
                    result?;
                }
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.chain.plan = Default::default();
                self.chain.dot_prompt_restore = None;
                self.renderer.chain_prompt = None;
                self.renderer.chain_but_mode = false;
                self.discard_interrupted_queue();
                self.renderer
                    .write_line("Plan not executed; still planning", C_AGENT)?;
            }
            KeyCode::Char('e' | 'E') => {
                self.input.discard_draft();
                self.input
                    .load_text(&self.chain.plan.pending.as_ref().unwrap().plan);
                self.chain.plan.editing = true;
                self.renderer.chain_prompt = None;
                self.renderer.chain_but_mode = true;
                self.renderer.write_line("Edit the plan; Enter previews it for approval, Esc returns to the original plan", C_AGENT)?;
            }
            _ => {}
        }
        self.refresh()?;
        Ok(true)
    }

    async fn start_main_run(
        &mut self,
        text: &str,
        question_origin: crate::question::QuestionOrigin,
    ) -> anyhow::Result<()> {
        #[cfg(feature = "mcp")]
        let prepared = self.mcp_resource_text(text);
        #[cfg(feature = "mcp")]
        let text = prepared.as_deref().unwrap_or(text);
        crate::startup::heap_future(|| self.start_main_run_prepared(text, question_origin)).await
    }

    /// Already projected literal input: no slash, bang, directives, MCP
    /// references or operator-approval reclassification occurs here.
    async fn start_main_run_prepared(
        &mut self,
        text: &str,
        question_origin: crate::question::QuestionOrigin,
    ) -> anyhow::Result<()> {
        #[cfg(feature = "mcp")]
        if self
            .mcp_admin
            .as_ref()
            .is_some_and(|state| state.work.is_some() || state.approval.is_some())
        {
            self.input.load_text(text);
            self.renderer.write_line("Finish or cancel the MCP operation before starting a model turn; input remains in the editor", C_ERROR)?;
            return Ok(());
        }
        #[cfg(feature = "git-worktree")]
        anyhow::ensure!(
            self.chain.git.work.is_none(),
            "Git command cleanup is still pending; no next turn admitted"
        );
        if self.budget_admission_notice()? {
            self.restore_interrupted_prompt(text);
            return Ok(());
        }
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
        let planning = crate::ui::plan::PlanRun::capture(
            self.ui.session,
            self.ui.context,
            &self.ui.workspace,
            &self.ui.permission,
            self.ui.cfg,
        );
        self.chain.plan = Default::default();
        start_main_run(
            text,
            true,
            question_origin,
            &mut self.run,
            &mut self.ui,
            &self.slash,
            &mut self.prebuild,
        )
        .await?;
        if self.run.is_running && self.run.pending_turn.is_some() {
            self.chain.plan.run = planning;
        }
        Ok(())
    }

    async fn start_internal_main_run(&mut self, text: &str) -> anyhow::Result<()> {
        #[cfg(feature = "mcp")]
        if self
            .mcp_admin
            .as_ref()
            .is_some_and(|state| state.work.is_some() || state.approval.is_some())
        {
            self.input.load_text(text);
            self.renderer.write_line("Finish or cancel the MCP operation before starting a model turn; input remains in the editor",C_ERROR)?;
            return Ok(());
        }
        if self.budget_admission_notice()? {
            return Ok(());
        }
        start_main_run(
            text,
            false,
            crate::question::QuestionOrigin::NewUserTurn,
            &mut self.run,
            &mut self.ui,
            &self.slash,
            &mut self.prebuild,
        )
        .await
    }

    fn budget_admission_notice(&mut self) -> anyhow::Result<bool> {
        if self.ui.cli.resolve_max_budget_usd(self.ui.cfg)?.is_none() {
            return Ok(false);
        }
        let run = crate::spend::run::Run::for_session(
            self.ui.session,
            self.ui.cfg,
            crate::spend::RequestKind::Turn,
        );
        if run.admit().is_ok() {
            return Ok(false);
        }
        if self.ui.session.spend_account.take_budget_notice() {
            self.renderer.write_line("USD budget: further model requests/effects stopped; queued input is retained. Use /cost to inspect recorded estimates.", C_ERROR)?;
        }
        Ok(true)
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
        question_origin: crate::question::QuestionOrigin,
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
        self.start_main_run(&msg, question_origin).await?;
        Ok(())
    }

    fn echo_main_submission(&mut self, text: &str) -> anyhow::Result<()> {
        for line in text.lines() {
            self.renderer
                .write_line(&format!("> {}", sanitize_output(line)), Color::Green)?;
        }
        self.renderer.write_line("", Color::White)?;
        Ok(())
    }

    async fn handle_dot_command(
        &mut self,
        text: &mut compact_str::CompactString,
    ) -> anyhow::Result<bool> {
        let action = super::dot_submission(text, &self.ui.context.prompts);
        if action == super::DotSubmission::Literal {
            return Ok(false);
        }
        // A one-shot returns its message to the caller, which echoes it once.
        if !matches!(action, super::DotSubmission::OneShot { .. }) {
            self.echo_main_submission(text)?;
        }
        if action == super::DotSubmission::Picker {
            self.input.buffer = ".".into();
            self.input.cursor = 1;
            self.input.start_dot_picker();
            return Ok(true);
        }

        if let super::DotSubmission::OneShot {
            prompt: prompt_name,
            message: msg,
        } = action
        {
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
        }
        if let super::DotSubmission::Switch(prompt_name) = action {
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
        } else if let super::DotSubmission::Unknown(prompt_name) = action {
            self.renderer
                .write_line(&format!("error: unknown prompt '{}'", prompt_name), C_ERROR)?;
            Ok(true)
        } else {
            unreachable!("literal, picker and one-shot dot input already returned")
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
        let Some(id) = self
            .btw_pricing
            .start(crate::print::HeadlessPricing::from_session(
                self.ui.session,
                self.ui.cfg,
            ))
        else {
            self.renderer.write_line(
                "[btw] side request capacity or request IDs exhausted",
                C_ERROR,
            )?;
            return Ok(());
        };
        let snapshot = crate::agent::runner::build_btw_snapshot_with_tool_result_retention(
            self.ui.session,
            &self.run.turn_trace,
            self.run.is_running,
            self.ui.cfg.resolve_keep_recent_tool_results(),
        );
        let model = self.ui.client.completion_model_with_cache_ttl(
            self.ui.session.model.to_string(),
            self.ui.cfg.cache_ttl.unwrap_or_default(),
        );
        let temperature =
            crate::config::resolve_temperature(self.ui.cli, self.ui.cfg, &self.ui.session.model);
        let extra_body = crate::config::resolve_extra_body(self.ui.cfg, &self.ui.session.model);
        let btw_agent = crate::startup::heap_future(|| {
            crate::provider::build_btw_agent(
                model,
                self.ui.cli,
                self.ui.cfg,
                self.ui.context,
                &self.ui.workspace,
                self.run
                    .agent
                    .as_ref()
                    .and_then(crate::provider::AnyAgent::context_snapshot)
                    .and_then(|snapshot| snapshot.instruction_sources.clone()),
                &self.ui.permission,
                &self.ui.ask_tx,
                &self.ui.session.id,
                self.ui.session.tool_result_spills.clone(),
                self.slash.reasoning_enabled,
                temperature,
                extra_body,
            )
        })
        .await;
        let runner = btw_agent
            .with_spend_for_session(self.ui.session, self.ui.cfg, crate::spend::RequestKind::Btw)
            .spawn_btw(
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
        #[cfg(feature = "mcp")]
        if crate::startup::heap_future(|| self.handle_mcp_command(text)).await? {
            return Ok(());
        }
        // The ordinary wrapper previously duplicated the measured large
        // selected-command future in the key-handler polling chain.
        crate::startup::heap_future(|| self.run_slash_command_selected(text, None)).await
    }

    async fn run_slash_command_selected(
        &mut self,
        text: &str,
        selected_session: Option<Box<Session>>,
    ) -> anyhow::Result<()> {
        #[cfg(feature = "agent-skills")]
        if self.handle_instruction_command(text).await? {
            return Ok(());
        }
        if self.run.is_running && !super::allowed_while_running(text) {
            self.renderer.write_line(
                "finish or stop the current turn before changing model, session or context",
                C_ERROR,
            )?;
            return Ok(());
        }
        if selected_session.is_some()
            && (self.run.is_running
                || self.permission_prompt.is_some()
                || self.run.pending_turn.is_some())
        {
            self.renderer.write_line(
                "finish or stop the current turn before resuming a session",
                C_ERROR,
            )?;
            return Ok(());
        }
        let mut command = text.split_whitespace();
        let name = command.next();
        let argument = command.next();
        if selected_session.is_some()
            || matches!(
                name,
                Some("/import" | "/new" | "/clear" | "/rename" | "/worktree")
            )
            || name == Some("/sessions")
                && argument.is_some_and(|arg| arg != "--all" && arg != "delete")
        {
            #[cfg(feature = "mcp")]
            if self
                .mcp_admin
                .as_ref()
                .is_some_and(|state| state.work.is_some() || state.approval.is_some())
            {
                self.renderer.write_line(
                    "Finish or cancel the MCP operation before changing sessions or workspaces",
                    C_ERROR,
                )?;
                return Ok(());
            }
            self.retire_session_title().await?;
        }

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
        // The selected-session branch makes this generic slash future large.
        // Build it in the short factory frame so it does not inflate every
        // user-event/event-loop polling frame, including unrelated commands.
        let prior_question_session = self.ui.session.id.clone();
        let had_pending_question = self.ui.session.pending_question.is_some();
        let result = crate::startup::heap_future(|| {
            crate::ui::slash::handle_slash(
                text,
                selected_session,
                &mut self.renderer,
                &mut self.input,
                &mut self.run,
                &mut self.ui,
                &mut self.slash,
                &mut self.chain,
                &mut self.terminal_guard,
            )
        })
        .await;
        if needs_tty {
            self.rebind_event_thread();
        }
        if prior_question_session != self.ui.session.id
            || (had_pending_question && self.ui.session.pending_question.is_none())
        {
            self.run.question_turn = None;
            #[cfg(feature = "agent-skills")]
            {
                self.run.instruction_turn = None;
                self.run.pending_skill_turn = None;
            }
        }

        if result.as_ref().is_err_and(|error| {
            error
                .downcast_ref::<crate::ui::terminal::TerminalLifecycleError>()
                .is_some()
        }) {
            return result;
        }

        if crate::ui::slash::is_readonly_view(text) || crate::ui::slash::is_jobs_control(text) {
            if let Err(error) = result {
                self.renderer
                    .write_line(&sanitize_output(&error.to_string()), C_ERROR)?;
            }
            return Ok(());
        }
        // /cost is a read-only query, including while a turn is live.
        if name == Some("/cost") {
            return result;
        }

        // The provider may have changed: warm its model list in the
        // background (never blocking the command) and show what is cached.
        let context_diagnostic = crate::ui::slash::is_readonly_diagnostic(text);
        // Successful diagnostics cannot enter the generic success settlement:
        // that saves history and may auto-start a pending first goal round.
        if context_diagnostic && result.is_ok() {
            return Ok(());
        }
        if !is_quit_command(text) && !context_diagnostic {
            let provider = self.ui.session.provider.to_string();
            let is_custom = self.ui.cfg.custom_providers_map().contains_key(&provider);
            crate::ui::slash::warm_model_cache(&provider, is_custom, &self.ui.client);
            self.sync_live_model_names(true);
        }

        // Keep the measured settlement future out of the command poll frame,
        // including commands that immediately return an exit/error result.
        crate::startup::heap_future(|| self.handle_slash_result(result)).await?;
        if !context_diagnostic {
            self.save_session()?;
        }
        Ok(())
    }

    #[cfg(feature = "agent-skills")]
    async fn handle_instruction_command(&mut self, text: &str) -> anyhow::Result<bool> {
        let Some(command) = text.split_whitespace().next() else {
            return Ok(false);
        };
        let Some(name) = command.strip_prefix('/') else {
            return Ok(false);
        };
        if name != "skills"
            && (crate::ui::pickers::list::reserved_command(name)
                || crate::context::commands::load_for_workspace(&self.ui.workspace)
                    .contains_key(name))
        {
            return Ok(false);
        }
        let services = self.ui.instruction_services()?;
        if name == "skills" {
            if text.trim() != "/skills" {
                self.renderer.write_line("usage: /skills", C_ERROR)?;
                return Ok(true);
            }
            let snapshot = if self.run.is_running || self.permission_prompt.is_some() {
                services.cached()
            } else {
                match services.refresh(true).await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        self.renderer
                            .write_line("skill catalogue unavailable", C_ERROR)?;
                        return Ok(true);
                    }
                }
            };
            let entries = snapshot
                .packages
                .iter()
                .filter(|(name, _)| !crate::ui::pickers::list::reserved_command(name))
                .map(|(name, package)| {
                    (
                        format!("/{name}"),
                        format!(
                            "{} — {}",
                            package.origin,
                            skill_description(&package.record.description)
                        ),
                    )
                })
                .collect::<Vec<_>>();
            self.input.set_skill_commands(entries.clone());
            self.renderer.write_line(
                "Agent Skills (instruction context; no permission grants):",
                C_TOOL,
            )?;
            for (name, description) in &entries {
                self.renderer.write_line(
                    &format!("  {}  {}", sanitize_output(name), description),
                    C_TOOL,
                )?;
            }
            if entries.is_empty() {
                self.renderer
                    .write_line("  no currently authorized skills", C_TOOL)?;
            }
            for diagnostic in snapshot.diagnostics.iter().take(16) {
                self.renderer
                    .write_line(&sanitize_output(diagnostic), C_ERROR)?;
            }
            if !entries.is_empty() {
                self.input.start_skills_picker();
            }
            return Ok(true);
        }
        // An unknown slash word only becomes a skill command when the exact
        // captured catalogue contains it; builtins/templates have precedence.
        let snapshot = if self.run.is_running || self.permission_prompt.is_some() {
            services.cached()
        } else {
            match services.refresh(true).await {
                Ok(snapshot) => snapshot,
                Err(_) => return Ok(false),
            }
        };
        if !snapshot.packages.contains_key(name) {
            return Ok(false);
        }
        if self.run.is_running
            || self.permission_prompt.is_some()
            || self.run.pending_turn.is_some()
        {
            self.renderer.write_line(
                "finish or stop the current turn before pinning a skill",
                C_ERROR,
            )?;
            return Ok(true);
        }
        let pin = match services.pin(name).await {
            Ok(pin) => pin,
            Err(error) => {
                self.renderer
                    .write_line(&sanitize_output(&error), C_ERROR)?;
                return Ok(true);
            }
        };
        let arguments = text
            .trim_start()
            .strip_prefix(command)
            .unwrap_or("")
            .trim_start();
        let message = if arguments.is_empty() {
            format!("Use the {name} skill.")
        } else {
            arguments.to_string()
        };
        let turn =
            crate::extras::skills::session::InstructionTurn::new(services, Some(pin.clone()));
        crate::ui::ensure_agent(
            &mut self.run.agent,
            &mut self.ui,
            self.slash.reasoning_enabled,
        )
        .await;
        let preflight = self
            .run
            .agent
            .as_ref()
            .unwrap()
            .preflight_instruction_turn(&message, Arc::clone(&turn))
            .await;
        if let Err(error) = preflight {
            self.renderer
                .write_line(&sanitize_output(&error), C_ERROR)?;
            return Ok(true);
        }
        self.run.pending_skill_turn = Some(turn);
        crate::startup::heap_future(|| {
            self.start_main_run_prepared(&message, crate::question::QuestionOrigin::NewUserTurn)
        })
        .await?;
        Ok(true)
    }

    fn refresh_custom_commands(&mut self) {
        let commands = crate::context::commands::load_for_workspace(&self.ui.workspace)
            .into_iter()
            .map(|(name, template)| (format!("/{name}"), template.description))
            .collect::<Vec<_>>();
        #[cfg(feature = "mcp")]
        let commands = {
            let mut commands = commands;
            let mut resources = Vec::new();
            if let Some(manager) = &self.ui.mcp_manager {
                for client in &manager.handles {
                    let (prompts, entries) = crate::extras::mcp::catalogue::cached_choices(client);
                    commands.extend(prompts);
                    resources.extend(entries);
                }
            }
            self.input.set_mcp_resources(resources);
            commands
        };
        self.input.set_custom_commands(commands);
    }

    async fn submit_custom_command(
        &mut self,
        command: crate::context::commands::DeferredCustomCommand,
    ) -> anyhow::Result<()> {
        let template = command.template;
        if template
            .mode
            .as_ref()
            .is_some_and(|mode| !self.ui.context.prompts.contains_key(mode))
        {
            self.renderer
                .write_line("custom command names an unavailable prompt mode", C_ERROR)?;
            return Ok(());
        }
        let message =
            match template.expand(&command.arguments, &self.ui.workspace, &self.ui.permission) {
                Ok(message) => message,
                Err(error) => {
                    self.renderer
                        .write_line(&format!("custom command: {error}"), C_ERROR)?;
                    return Ok(());
                }
            };
        let reserve = self.ui.cfg.resolve_reserve_tokens(
            &self.ui.session.model,
            &config::quick_models_map(self.ui.cfg),
            self.ui.session.context_window,
        );
        if self
            .ui
            .session
            .is_irreducible_with_pending(reserve, Session::estimate_tokens(&message))
        {
            self.renderer.write_line(
                "expanded custom command is too large for the current context",
                C_ERROR,
            )?;
            return Ok(());
        }
        if let Some(model) = &template.model {
            let model_command = format!("/model {model}");
            let result = crate::startup::heap_future(|| {
                crate::ui::slash::handle_slash(
                    &model_command,
                    None,
                    &mut self.renderer,
                    &mut self.input,
                    &mut self.run,
                    &mut self.ui,
                    &mut self.slash,
                    &mut self.chain,
                    &mut self.terminal_guard,
                )
            })
            .await;
            if let Err(error) = result {
                self.renderer.write_line(
                    &format!(
                        "custom command model: {}",
                        sanitize_output(&error.to_string())
                    ),
                    C_ERROR,
                )?;
                return Ok(());
            }
        }
        if template.mode.is_some() || template.security_mode.is_some() {
            self.chain.dot_prompt_restore = Some(self.ui.context.active_selection());
        }
        if let Some(mode) = &template.mode {
            if let Some(permission) = &self.ui.permission {
                permission
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_prompt_grant_offer(None);
            }
            // Activate authored prompt selection without arming any embedded
            // grant offer: templates cannot create implicit permission grants.
            let directive = self.ui.context.activate_prompt(mode).flatten();
            super::apply_mode_directive(directive.as_deref(), &self.ui.permission);
            if template.model.is_none() {
                apply_prompt_model(
                    mode,
                    &mut self.ui,
                    &mut self.run.agent,
                    self.slash.reasoning_enabled,
                    &mut self.renderer,
                )
                .await;
            }
        }
        super::apply_mode_directive(template.security_mode.as_deref(), &self.ui.permission);
        self.ui.invalidate_prebuild();
        self.run.agent = None;
        self.echo_main_submission(&message)?;
        self.start_main_run(&message, crate::question::QuestionOrigin::NewUserTurn)
            .await?;
        if !self.run.is_running
            && let Some(selection) = self.chain.dot_prompt_restore.take()
        {
            self.ui.context.restore_selection(selection);
            if let Some(permission) = &self.ui.permission {
                permission
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .restore_user_mode();
            }
        }
        Ok(())
    }

    async fn handle_slash_result(&mut self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        match result {
            Err(e)
                if e.downcast_ref::<crate::context::commands::DeferredCustomCommand>()
                    .is_some() =>
            {
                let command = e
                    .downcast::<crate::context::commands::DeferredCustomCommand>()
                    .unwrap();
                // Keep the template child future out of the generic slash
                // result poll frame, which also carries session-resume work.
                crate::startup::heap_future(|| self.submit_custom_command(command)).await?;
            }
            #[cfg(feature = "git-worktree")]
            Err(e)
                if e.downcast_ref::<crate::ui::slash::git::DeferredCommand>()
                    .is_some() =>
            {
                let command = e
                    .downcast::<crate::ui::slash::git::DeferredCommand>()
                    .unwrap();
                if let Err(error) =
                    crate::startup::heap_future(|| self.begin_git_command(&command.0)).await
                {
                    self.renderer.write_line(
                        &format!(
                            "Git command rejected: {}",
                            sanitize_output(&error.to_string())
                        ),
                        C_ERROR,
                    )?;
                }
            }
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
                crate::startup::heap_future(|| self.start_mcp_login(server)).await?;
            }
            #[cfg(feature = "git-worktree")]
            Err(e)
                if e.downcast_ref::<crate::extras::git_worktree::DeferredWorktreeAction>()
                    .is_some() =>
            {
                self.retire_session_title().await?;
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
                self.run.question_turn = Some(crate::question::QuestionTurn::default());
                self.ui.session.pending_question = None;
                #[cfg(feature = "agent-skills")]
                {
                    self.run.instruction_turn =
                        Some(crate::extras::skills::session::InstructionTurn::new(
                            self.ui.instruction_services()?,
                            None,
                        ));
                }
                let agent = self.run.agent.as_ref().unwrap().clone();
                #[cfg(feature = "agent-skills")]
                let agent = agent.with_instruction_turn(self.run.instruction_turn.clone());
                let runner = agent
                    .with_spend_for_session(
                        self.ui.session,
                        self.ui.cfg,
                        crate::spend::RequestKind::Turn,
                    )
                    .with_question_turn(self.run.question_turn.clone())
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
                // `/loop` sets a goal and expects its first round to start
                // without further input; an ordinary `/goal` waits for the
                // user's next message.
                #[cfg(feature = "goal")]
                if !self.run.is_running
                    && let Some(goal) = self.ui.session.goal_store.snapshot().filter(|g| {
                        g.context_file.is_some() && g.status.is_running() && g.progress.rounds == 0
                    })
                {
                    self.ensure_agent().await;
                    self.run.request_tool_results_cleared = 0;
                    let prompt = crate::extras::goal::driver::first_round_prompt(&goal);
                    self.run.question_turn = Some(crate::question::QuestionTurn::default());
                    self.ui.session.pending_question = None;
                    #[cfg(feature = "agent-skills")]
                    {
                        self.run.instruction_turn =
                            Some(crate::extras::skills::session::InstructionTurn::new(
                                self.ui.instruction_services()?,
                                None,
                            ));
                    }
                    let agent = self.run.agent.as_ref().unwrap().clone();
                    #[cfg(feature = "agent-skills")]
                    let agent = agent.with_instruction_turn(self.run.instruction_turn.clone());
                    let runner = agent
                        .with_spend_for_session(
                            self.ui.session,
                            self.ui.cfg,
                            crate::spend::RequestKind::Turn,
                        )
                        .with_question_turn(self.run.question_turn.clone())
                        .spawn_runner(
                            prompt,
                            Vec::new(),
                            self.ui.cfg.retry.clone(),
                            #[cfg(feature = "hooks")]
                            None,
                        )
                        .await;
                    self.run.compaction_decision_tx = runner.compaction_decision_tx;
                    self.run.agent_rx = Some(runner.event_rx);
                    self.run.main_abort = Some(runner.abort_handle);
                    self.run.is_running = true;
                    self.run.goal_round = Some(crate::extras::goal::driver::RoundCollector::new());
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
                    } else if matches!(event, UserEvent::Quit) {
                        cancellation.cancel();
                        self.deferred_user_events.push_back(event);
                    } else {
                        self.deferred_user_events.push_back(event);
                    }
                }
            }
        };
        let result = run.rendered_output();

        self.ui.session.add_shell_interaction(text, &result);
        if !self.ui.cli.no_session {
            let _ = crate::session::chat_history::append_entry(
                &crate::session::chat_history::ChatHistoryEntry {
                    content: text.to_string(),
                    timestamp: self.ui.session.updated_at.clone(),
                },
            );
        }
        // The command already ran and its process cleanup is complete. Keep
        // its transcript durable before fallible terminal presentation.
        self.save_session()?;

        for line in result.lines() {
            let safe_line = sanitize_output(line);
            self.renderer
                .write_line(&safe_line, if run.succeeded() { C_AGENT } else { C_ERROR })?;
        }
        self.renderer.write_line("", Color::White)?;

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
        self.cancel_pending_permissions();
        let preserve_progress = preserve_pending_main_turn_progress(&mut self.run, self.ui.session);
        self.discard_interrupted_queue();
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
                self.restore_interrupted_prompt(&text);
            }
            self.save_session()?;
        }
        rendered
    }

    /// Title work has a retained join handle, captured prices/root/generation,
    /// and once-only usage. Main input and streams remain independently live.
    fn request_session_title(&mut self) {
        if self.ui.cli.no_session
            || self.title_work.is_some()
            || self.title_requested_for.as_deref() == Some(self.ui.session.id.as_str())
        {
            return;
        }
        let Some(request) = crate::session::title::title_request(self.ui.cfg, self.ui.session)
        else {
            return;
        };
        if self.ui.workspace.validate().is_err() {
            return;
        }
        self.title_requested_for = Some(request.session_id.clone());
        self.title_generation = self.title_generation.wrapping_add(1);
        let generation = self.title_generation;
        let client = if request.provider == self.ui.session.provider {
            Ok(self.ui.client.clone())
        } else {
            crate::provider::create_client(
                &request.provider,
                None,
                &self.ui.cfg.custom_providers_map(),
                self.ui.cfg.api_keys.as_ref(),
            )
        };
        let pricing =
            crate::session::title::TitlePricing::capture(self.ui.cfg, self.ui.session, &request);
        let spend = crate::spend::run::Run::for_session(
            self.ui.session,
            self.ui.cfg,
            crate::spend::RequestKind::Title,
        )
        .with_captured_profile(
            &request.provider,
            &request.model,
            crate::spend::PriceSnapshot::capture(
                pricing.known_prices(),
                pricing.cache,
                pricing.anthropic_native,
            ),
            self.ui.cfg,
            crate::spend::RequestKind::Title,
        );
        if spend.admit().is_err() {
            let _ = self.renderer.write_line(
                "USD budget: optional title skipped; completed answer retained",
                C_ERROR,
            );
            return;
        }
        let ledger = crate::session::title::UsageLedger::with_spend(spend.clone());
        let observed = ledger.clone();
        let cfg = self.ui.cfg.clone();
        let tx = self.title_tx.clone();
        let id = request.session_id.clone();
        let provider = request.provider.clone();
        let work_scope = crate::agent::runner::AgentWorkScope::new();
        let task_scope = work_scope.clone();
        let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(work_scope.clone());
        let task = tokio::spawn(async move {
            let title = task_scope
                .run(async {
                    match client {
                        Ok(client) => {
                            crate::spend::run::scope(
                                Some(spend),
                                crate::session::title::generate(&request, &client, &cfg, observed),
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    }
                })
                .await;
            cleanup.settle().await;
            let _ = tx
                .send(SessionTitleResult {
                    generation,
                    session_id: request.session_id,
                    title,
                })
                .await;
        });
        self.title_work = Some(SessionTitleWork {
            session_id: id,
            generation,
            workspace: self.ui.workspace.clone(),
            provider,
            pricing,
            ledger,
            work_scope,
            task: Some(task),
        });
    }

    async fn settle_session_title(&mut self, cancel: bool) -> anyhow::Result<()> {
        let Some(mut work) = self.title_work.take() else {
            return Ok(());
        };
        if let Some(mut task) = work.task.take() {
            if cancel {
                task.abort();
            }
            // Abort stops publication; its retained scope still owns any
            // in-flight ledger writer and must settle before retirement.
            if tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        work.work_scope.cancellation_handle().cancel();
        // Financial usage is projected before cleanup: a stalled disk cannot
        // erase an already-observed title charge or keep input unresponsive.
        anyhow::ensure!(
            work.session_id == self.ui.session.id
                && Arc::ptr_eq(&work.workspace, &self.ui.workspace),
            "title ownership changed before settlement"
        );
        let usage = self
            .ui
            .cfg
            .normalize_usage(&work.provider, work.ledger.take());
        work.pricing.charge(self.ui.session, usage);
        if usage.has_values() {
            self.save_session()?;
        }
        tokio::time::timeout(Duration::from_secs(5), work.work_scope.wait_idle())
            .await
            .map_err(|_| anyhow::anyhow!("title ledger writer is still settling"))?;
        Ok(())
    }

    async fn retire_session_title(&mut self) -> anyhow::Result<()> {
        self.title_generation = self.title_generation.wrapping_add(1);
        self.settle_session_title(true).await
    }

    async fn apply_session_title(&mut self, titled: SessionTitleResult) -> anyhow::Result<()> {
        if titled.generation != self.title_generation || titled.session_id != self.ui.session.id {
            return Ok(());
        }
        if let Some(work) = &self.title_work
            && (work.generation != titled.generation
                || !Arc::ptr_eq(&work.workspace, &self.ui.workspace))
        {
            return Ok(());
        }
        self.settle_session_title(false).await?;
        self.ui.workspace.validate().map_err(anyhow::Error::msg)?;
        match titled.title {
            Ok(title) => {
                if crate::session::title::apply(self.ui.session, &titled.session_id, title) {
                    self.save_session()?;
                }
            }
            Err(error) => {
                tracing::debug!(%error,"session title unavailable; keeping first-message fallback")
            }
        }
        Ok(())
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
                let Some(pricing) = self.btw_pricing.take(id) else {
                    return Ok(());
                };
                self.btw_total_cost += pricing.cost(rig::completion::Usage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    cache_creation_input_tokens,
                    ..rig::completion::Usage::default()
                });
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
                if self.btw_pricing.take(id).is_none() {
                    return Ok(());
                }
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
        self.renderer.forget_title();
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
                self.btw_pricing.cancel_all();
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
        #[cfg(feature = "agent-skills")]
        {
            self.run.instruction_turn = None;
            self.run.pending_skill_turn = None;
            self.input.set_skill_commands(Vec::new());
        }

        self.refresh_custom_commands();
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
        #[cfg(feature = "goal")]
        let mut run = AgentRunState::default();
        let mut deferred = std::collections::VecDeque::from([UserEvent::Paste("earlier".into())]);
        // Exercise a second reader on the same event channel after a handoff.
        for round in 0..2 {
            // A goal round's verification is in flight across the handoff,
            // registered exactly as `start_goal_verification` registers it.
            #[cfg(feature = "goal")]
            let (operation_id, _cancelled) = {
                run.goal_gate_generation += 1;
                let operation_id = crate::event::ValidationOperationId(run.goal_gate_generation);
                let (cancel, cancelled) = tokio::sync::oneshot::channel();
                run.pending_goal_gate = Some(crate::ui::state::PendingGoalGate {
                    operation_id,
                    cancel,
                    goal_id: "handoff-goal".into(),
                    summary: crate::extras::goal::gate::RoundSummary::completed(),
                    request: crate::extras::goal::gate::VerifyRequest {
                        run_verify_command: false,
                        run_checks: true,
                        run_judge: false,
                        cause: crate::extras::goal::gate::VerifyCause::MetClaim,
                    },
                });
                (operation_id, cancelled)
            };
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
            #[cfg(feature = "goal")]
            let completion = {
                assert!(run.validation_active());
                UserEvent::GoalVerificationDone(Box::new(crate::event::GoalVerificationEvent {
                    operation_id,
                    checks: Some(crate::extras::goal::gate::CheckOutcome {
                        all_passed: true,
                        failure_tail: Some(format!("completion-{round}")),
                        verified: Vec::new(),
                    }),
                    judge: None,
                    judge_tokens: 0,
                    judge_charge: None,
                    interrupted: false,
                }))
            };
            #[cfg(not(feature = "goal"))]
            let completion = UserEvent::LinkOpenFailed(format!("completion-{round}"));
            background.blocking_send(completion).unwrap();
            let completion = receiver.blocking_recv().unwrap();
            // The completion still settles the round registered before the
            // handoff, and only that round.
            #[cfg(feature = "goal")]
            {
                let UserEvent::GoalVerificationDone(event) = &completion else {
                    panic!("unexpected completion: {completion:?}");
                };
                assert!(
                    run.pending_goal_gate
                        .take_if(|pending| pending.operation_id == event.operation_id)
                        .is_some()
                );
                assert!(!run.validation_active());
            }
            deferred.push_back(completion);
            pause_input_reader(&running, &mut handle, &mut receiver, &mut deferred);
        }
        let actual: Vec<_> = deferred
            .into_iter()
            .map(|event| match event {
                UserEvent::Paste(text) | UserEvent::LinkOpenFailed(text) => text,
                #[cfg(feature = "goal")]
                UserEvent::GoalVerificationDone(event) => event
                    .checks
                    .and_then(|outcome| outcome.failure_tail)
                    .unwrap_or_default(),
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

#[cfg(test)]
mod interrupt_rewind_tests {
    use super::test_support::fixture_app;
    use super::*;
    use crate::session::MessageRole;

    #[tokio::test]
    async fn actual_rewind_confirmation_restores_shell_command_in_editor() {
        let root = crate::tests::OwnedTestDirectory::new("app-shell-rewind");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("openrouter", "test-model", 128_000, "");
        session.add_message(MessageRole::User, "earlier prompt");
        session.add_message(MessageRole::Assistant, "earlier answer");
        session.add_shell_interaction("!printf héllo", "CAPTURED_SHELL_CONTENTS");
        session.add_message(MessageRole::User, "later prompt");
        let before = serde_json::to_value(&session.messages).unwrap();
        let ui = UiContext {
            cli: &cli,
            cfg: &cfg,
            session: &mut session,
            context: &mut context,
            workspace,
            client: crate::provider::create_client(
                "openrouter",
                Some("unused-test-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox: crate::sandbox::Sandbox::new(false, "none"),
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        let mut app = fixture_app(ui);
        app.input
            .start_rewind_picker(crate::ui::rewind_targets(app.ui.session));
        for code in [KeyCode::Down, KeyCode::Enter, KeyCode::Enter] {
            app.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
                .await
                .unwrap();
        }
        assert_eq!(app.input.buffer, "!printf héllo");
        assert_eq!(app.ui.session.messages.len(), 2);
        assert!(app.ui.session.redo());
        assert_eq!(
            serde_json::to_value(&app.ui.session.messages).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn actual_provider_error_restores_draft_without_starting_queued_requests() {
        let root = crate::tests::OwnedTestDirectory::new("app-provider-error-draft");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "custom_providers": {"error-fixture": {"provider_type":"openai", "base_url":"http://127.0.0.1:1", "api_style":"completions"}}
        })).unwrap();
        for progress in [false, true] {
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new("error-fixture", "test-model", 128_000, "");
            session.add_message(MessageRole::User, "earlier prompt");
            session.add_message(MessageRole::Assistant, "earlier answer");
            let ui = UiContext {
                cli: &cli,
                cfg: &cfg,
                session: &mut session,
                context: &mut context,
                workspace,
                client: crate::provider::create_client(
                    "error-fixture",
                    Some("unused-test-key"),
                    &cfg.custom_providers_map(),
                    cfg.api_keys.as_ref(),
                )
                .unwrap(),
                permission: None,
                ask_tx: None,
                sandbox: crate::sandbox::Sandbox::new(false, "none"),
                prebuild_invalidated: Arc::new(AtomicBool::new(false)),
                status_signals: None,
                #[cfg(feature = "skills")]
                skill_services: Arc::new(
                    crate::extras::js::skills::session::SkillServiceOwner::new(),
                ),
                #[cfg(feature = "mcp")]
                mcp_manager: None,
            };
            let mut app = fixture_app(ui);
            let pending = PendingMainTurn::capture(app.ui.session, "failed provider prompt");
            crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
            if progress {
                app.ui.session.add_tool_call_with_id(
                    "provider-error-read",
                    "read",
                    &serde_json::json!({"path":"fixture.txt"}),
                );
            }
            app.input.load_text("follow-up after provider failure");
            app.run.pending_inputs.extend([
                "must not call provider first".into(),
                "must not call provider second".into(),
            ]);
            app.handle_agent_event(AgentEvent::Error {
                message: "fixture provider error".into(),
                interactions: Vec::new(),
            })
            .await
            .unwrap();
            // A queued run would install its agent, task and pending user turn
            // before sending a provider request. All admission state stays idle.
            assert!(!app.run.is_running);
            assert!(app.run.agent.is_none());
            assert!(app.run.main_abort.is_none());
            assert!(app.run.pending_turn.is_none());
            assert!(app.run.pending_inputs.is_empty());
            assert!(
                !app.ui
                    .session
                    .messages
                    .iter()
                    .any(|message| message.content.starts_with("must not call provider"))
            );
            app.run_queue_command("").unwrap();
            let lines = app.renderer.feed().line_count(4096);
            let shown = app
                .renderer
                .feed()
                .selected_text(4096, 0, lines - 1)
                .unwrap();
            assert!(shown.contains("dropped 2 queued inputs"), "{shown}");
            assert!(shown.contains("queue is empty"), "{shown}");
            if progress {
                assert_eq!(app.input.buffer, "follow-up after provider failure");
                assert!(
                    app.ui
                        .session
                        .messages
                        .iter()
                        .any(|message| message.role == MessageRole::ToolResult)
                );
            } else {
                assert_eq!(app.input.buffer, "failed provider prompt");
                assert_eq!(app.ui.session.messages.len(), 2);
                app.input.clear_buffer();
                app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
                    .await
                    .unwrap();
                assert_eq!(app.input.buffer, "follow-up after provider failure");
            }
        }
    }

    #[tokio::test]
    async fn actual_context_exhaustion_preserves_followup_draft_usage_and_drops_queue() {
        let root = crate::tests::OwnedTestDirectory::new("app-context-stop-draft");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "ZS_CREDENTIALS_DIR",
                Some(root.path().join("credentials").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "compact_enabled": true, "mid_turn_compact_threshold": 0.8
        }))
        .unwrap();
        for progress in [false, true] {
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new("openrouter", "test-model", 128_000, "");
            session.working_dir = workspace.root().to_string_lossy().into();
            session.add_message(MessageRole::User, "earlier prompt");
            session.add_message(MessageRole::Assistant, "earlier answer");
            session.set_calibration(100, 20);
            session.total_input_tokens = 10;
            session.total_output_tokens = 2;
            session.total_real_input_tokens = 10;
            session.input_token_cost = 2.0;
            session.output_token_cost = 4.0;
            session.total_cost = 0.5;
            let before = session.clone();
            let ui = UiContext {
                cli: &cli,
                cfg: &cfg,
                session: &mut session,
                context: &mut context,
                workspace,
                client: crate::provider::create_client(
                    "openrouter",
                    Some("unused-test-key"),
                    &cfg.custom_providers_map(),
                    cfg.api_keys.as_ref(),
                )
                .unwrap(),
                permission: None,
                ask_tx: None,
                sandbox: crate::sandbox::Sandbox::new(false, "none"),
                prebuild_invalidated: Arc::new(AtomicBool::new(false)),
                status_signals: None,
                #[cfg(feature = "skills")]
                skill_services: Arc::new(
                    crate::extras::js::skills::session::SkillServiceOwner::new(),
                ),
                #[cfg(feature = "mcp")]
                mcp_manager: None,
            };
            let mut app = fixture_app(ui);
            let pending = PendingMainTurn::capture(app.ui.session, "context-exhausted prompt");
            crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
            app.run.is_running = true;
            app.run.awaiting_compaction_relief = true;
            if progress {
                app.ui.session.add_tool_call_with_id(
                    "context-stop-read",
                    "read",
                    &serde_json::json!({"path":"fixture.txt"}),
                );
                app.run.response_buf = "observed partial answer".into();
            }
            app.input
                .load_text("follow-up draft héllo after context exhaustion");
            app.run.pending_inputs.extend([
                "must not start queued first".into(),
                "must not start queued second".into(),
            ]);
            app.handle_agent_event(AgentEvent::UsageDelta {
                usage: crate::event::UsageDelta {
                    input_tokens: 110_000,
                    output_tokens: 100,
                    total_tokens: 110_100,
                    ..Default::default()
                },
                context_complete: true,
                awaits_compaction_decision: true,
            })
            .await
            .unwrap();
            assert!(!app.run.is_running);
            assert!(app.run.pending_turn.is_none());
            assert!(app.run.main_abort.is_none());
            assert!(app.run.compaction_decision_tx.is_none());
            assert!(!app.run.awaiting_compaction_relief);
            assert!(app.run.pending_inputs.is_empty());
            // Exercise the ordinary queue admission point too: context stops
            // must never launch the next queued prompt on a later idle tick.
            app.finalize_turn().await.unwrap();
            assert!(app.run.agent.is_none());
            assert!(app.run.pending_turn.is_none());
            assert!(app.run.main_abort.is_none());
            assert!(
                !app.ui
                    .session
                    .messages
                    .iter()
                    .any(|message| message.content.starts_with("must not start queued"))
            );
            assert_eq!(app.ui.session.total_input_tokens, 110_010);
            assert_eq!(app.ui.session.total_output_tokens, 102);
            assert_eq!(app.ui.session.total_real_input_tokens, 110_010);
            assert!((app.ui.session.total_cost - 0.7204).abs() < 1e-12);
            let persisted = crate::session::storage::load_session_exact(&app.ui.session.id)
                .unwrap()
                .expect("context-stop settlement persists the session");
            assert_eq!(
                serde_json::to_value(&persisted.messages).unwrap(),
                serde_json::to_value(&app.ui.session.messages).unwrap()
            );
            assert_eq!(persisted.total_input_tokens, 110_010);
            assert!((persisted.total_cost - 0.7204).abs() < 1e-12);
            app.run_queue_command("").unwrap();
            let lines = app.renderer.feed().line_count(4096);
            let shown = app
                .renderer
                .feed()
                .selected_text(4096, 0, lines - 1)
                .unwrap();
            assert!(
                shown.contains("not enough context to continue this turn"),
                "{shown}"
            );
            assert!(shown.contains("dropped 2 queued inputs"), "{shown}");
            assert!(shown.contains("queue is empty"), "{shown}");
            if progress {
                assert_eq!(
                    app.input.buffer,
                    "follow-up draft héllo after context exhaustion"
                );
                assert_eq!(
                    app.ui.session.messages.last().unwrap().content,
                    "observed partial answer"
                );
                assert!(app.ui.session.messages.iter().any(|message| {
                    message.role == MessageRole::ToolResult
                        && message.tool_call_id.as_deref() == Some("context-stop-read")
                        && message
                            .content
                            .contains(crate::agent::runner::UNKNOWN_TOOL_OUTCOME)
                }));
                assert_eq!(app.ui.session.calibrated_tokens, 110_100);
            } else {
                assert_eq!(app.input.buffer, "context-exhausted prompt");
                assert_eq!(
                    serde_json::to_value(&app.ui.session.messages).unwrap(),
                    serde_json::to_value(&before.messages).unwrap()
                );
                assert_eq!(
                    app.ui.session.total_estimated_tokens,
                    before.total_estimated_tokens
                );
                assert_eq!(app.ui.session.calibrated_tokens, before.calibrated_tokens);
                app.input.clear_buffer();
                app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
                    .await
                    .unwrap();
                assert_eq!(
                    app.input.buffer,
                    "follow-up draft héllo after context exhaustion"
                );
            }
        }
    }

    #[tokio::test]
    async fn actual_abort_and_failure_preserve_drafts_and_report_discarded_queue() {
        let root = crate::tests::OwnedTestDirectory::new("app-interrupt-drafts");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        for fail in [false, true] {
            for progress in [false, true] {
                let workspace =
                    Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
                let mut context =
                    crate::context::load(true).for_workspace_binding(true, &workspace);
                let mut session = Session::new("openrouter", "test-model", 128_000, "");
                session.add_message(MessageRole::User, "earlier prompt");
                session.add_message(MessageRole::Assistant, "earlier answer");
                let ui = UiContext {
                    cli: &cli,
                    cfg: &cfg,
                    session: &mut session,
                    context: &mut context,
                    workspace,
                    client: crate::provider::create_client(
                        "openrouter",
                        Some("unused-test-key"),
                        &cfg.custom_providers_map(),
                        cfg.api_keys.as_ref(),
                    )
                    .unwrap(),
                    permission: None,
                    ask_tx: None,
                    sandbox: crate::sandbox::Sandbox::new(false, "none"),
                    prebuild_invalidated: Arc::new(AtomicBool::new(false)),
                    status_signals: None,
                    #[cfg(feature = "skills")]
                    skill_services: Arc::new(
                        crate::extras::js::skills::session::SkillServiceOwner::new(),
                    ),
                    #[cfg(feature = "mcp")]
                    mcp_manager: None,
                };
                let mut app = fixture_app(ui);
                let pending = PendingMainTurn::capture(app.ui.session, "interrupted prompt");
                crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
                if progress {
                    app.ui.session.add_tool_call_with_id(
                        "interrupted-read",
                        "read",
                        &serde_json::json!({"path":"fixture.txt"}),
                    );
                }
                app.input.load_text("follow-up draft héllo");
                app.run
                    .pending_inputs
                    .extend(["queued first".into(), "queued second".into()]);
                if fail {
                    app.fail_pending_main_turn().await;
                } else {
                    app.abort_main_run().await.unwrap();
                }
                assert!(!app.run.is_running);
                assert!(app.run.pending_inputs.is_empty());
                app.run_queue_command("").unwrap();
                let lines = app.renderer.feed().line_count(4096);
                let shown = app
                    .renderer
                    .feed()
                    .selected_text(4096, 0, lines - 1)
                    .unwrap();
                assert!(shown.contains("dropped 2 queued inputs"), "{shown}");
                assert!(shown.contains("queue is empty"), "{shown}");
                if progress {
                    assert_eq!(app.input.buffer, "follow-up draft héllo");
                    assert!(
                        app.ui
                            .session
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::ToolResult)
                    );
                } else {
                    assert_eq!(app.input.buffer, "interrupted prompt");
                    assert_eq!(app.ui.session.messages.len(), 2);
                    app.input.clear_buffer();
                    app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
                        .await
                        .unwrap();
                    assert_eq!(app.input.buffer, "follow-up draft héllo");
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod explicit_shell_persistence_tests {
    use super::test_support::fixture_app;
    use super::*;
    use crate::session::MessageRole;

    struct SessionRelease(String);

    impl Drop for SessionRelease {
        fn drop(&mut self) {
            crate::session::lock::release_session(&self.0);
        }
    }

    fn shell_ui<'a>(
        cli: &'a crate::cli::Cli,
        cfg: &'a crate::config::Config,
        session: &'a mut Session,
        context: &'a mut crate::context::ContextFiles,
        workspace: Arc<crate::paths::WorkspaceBinding>,
    ) -> UiContext<'a> {
        let shell =
            crate::sandbox::ShellCapability::resolve("/bin/sh", workspace.root(), None).unwrap();
        let sandbox = crate::sandbox::Sandbox::new(false, "none")
            .with_workspace_binding(workspace.clone())
            .with_resolved_shell(Some(shell));
        UiContext {
            cli,
            cfg,
            session,
            context,
            workspace,
            client: crate::provider::create_client(
                "openrouter",
                Some("unused-test-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox,
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        }
    }

    #[tokio::test]
    async fn actual_explicit_shell_success_and_failure_are_saved_before_quit_unless_ephemeral() {
        let root = crate::tests::OwnedTestDirectory::new("explicit-shell-persistence");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "ZS_CREDENTIALS_DIR",
                Some(root.path().join("credentials").into_os_string()),
            ),
        ]);
        let cfg = crate::config::Config::default();
        for ephemeral in [false, true] {
            for (command, marker) in [
                ("!printf DURABLE_SHELL_SUCCESS", "DURABLE_SHELL_SUCCESS"),
                (
                    "!printf DURABLE_SHELL_FAILURE; exit 7",
                    "DURABLE_SHELL_FAILURE",
                ),
            ] {
                let cli = crate::cli::Cli {
                    no_session: ephemeral,
                    no_sandbox: true,
                    tools: vec!["shell".into()],
                    ..Default::default()
                };
                let workspace =
                    Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
                let mut context =
                    crate::context::load(true).for_workspace_binding(true, &workspace);
                let mut session = Session::new("openrouter", "fixture", 128_000, "");
                session.working_dir = workspace.root().to_string_lossy().into();
                session.total_cost = 0.125;
                session.add_message(MessageRole::User, "earlier turn");
                let id = session.id.clone();
                let _session_release = SessionRelease(id.to_string());
                let ui = shell_ui(&cli, &cfg, &mut session, &mut context, workspace);
                let mut app = fixture_app(ui);
                app.run_bang_command(command).await.unwrap();
                assert_eq!(app.ui.session.messages.len(), 2);
                let recorded = app.ui.session.messages.last().unwrap();
                assert_eq!(recorded.role, MessageRole::User);
                assert_eq!(
                    crate::session::shell_interaction_command(&recorded.content),
                    Some(command)
                );
                let body = recorded.content.split_once("`:\n```\n").unwrap().1;
                assert!(body.starts_with(marker), "captured shell body: {body}");
                let restored = crate::session::storage::load_session_exact(&id).unwrap();
                if ephemeral {
                    assert!(restored.is_none());
                } else {
                    let restored = restored.unwrap();
                    assert_eq!(
                        serde_json::to_value(&restored.messages).unwrap(),
                        serde_json::to_value(&app.ui.session.messages).unwrap()
                    );
                    assert_eq!(restored.total_cost, 0.125);
                }
            }
        }
    }

    #[tokio::test]
    async fn actual_explicit_shell_save_failure_keeps_live_record_and_warns() {
        let root = crate::tests::OwnedTestDirectory::new("explicit-shell-save-failure");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "ZS_CREDENTIALS_DIR",
                Some(root.path().join("credentials").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_sandbox: true,
            tools: vec!["shell".into()],
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("openrouter", "fixture", 128_000, "");
        let _session_release = SessionRelease(session.id.to_string());
        let blocked = crate::session::storage::session_path(&session.id).unwrap();
        std::fs::create_dir_all(&blocked).unwrap();
        let ui = shell_ui(&cli, &cfg, &mut session, &mut context, workspace);
        let mut app = fixture_app(ui);
        app.run_bang_command("!printf LIVE_SHELL_RECORD")
            .await
            .unwrap();
        assert_eq!(app.ui.session.messages.len(), 1);
        let body = app.ui.session.messages[0]
            .content
            .split_once("`:\n```\n")
            .unwrap()
            .1;
        assert!(
            body.starts_with("LIVE_SHELL_RECORD"),
            "captured shell body: {body}"
        );
        assert!(blocked.is_dir());
        let lines = app.renderer.feed().line_count(4096);
        let shown = app
            .renderer
            .feed()
            .selected_text(4096, 0, lines - 1)
            .unwrap();
        assert!(
            shown.contains("warning: failed to save session:"),
            "{shown}"
        );
        assert!(shown.contains("LIVE_SHELL_RECORD"), "{shown}");
    }
}

#[cfg(test)]
mod permission_loop_tests {
    use super::test_support::fixture_app;
    use super::*;
    use crate::permission::ask::{AskRequest, UserDecision};
    use crate::session::MessageRole;
    use tokio::sync::oneshot;

    /// Test-owned tasks are aborted even when an assertion unwinds. No native
    /// processes or network requests are launched by this channel fixture.
    struct OwnedRunner(Option<tokio::task::JoinHandle<()>>);
    impl Drop for OwnedRunner {
        fn drop(&mut self) {
            if let Some(task) = &self.0 {
                task.abort();
            }
        }
    }
    impl OwnedRunner {
        async fn joined(mut self) {
            let result = tokio::time::timeout(Duration::from_secs(2), self.0.as_mut().unwrap())
                .await
                .expect("exact fixture runner did not settle after cancellation");
            // Keep the handle in the RAII guard until the await completes, so a
            // deadline/assertion failure still aborts rather than detaches it.
            self.0.take();
            assert!(
                result.unwrap_err().is_cancelled(),
                "the exact owned run must be aborted"
            );
        }
    }
    struct Retired(Arc<AtomicBool>);
    impl Drop for Retired {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn environment(root: &crate::tests::OwnedTestDirectory) -> crate::tests::ScopedProcessEnv {
        crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "ZS_CREDENTIALS_DIR",
                Some(root.path().join("credentials").into_os_string()),
            ),
        ])
    }
    fn fixture_config() -> config::Config {
        serde_json::from_value(serde_json::json!({
            "custom_providers": {"permission-fixture": {"provider_type":"openai", "base_url":"http://127.0.0.1:1", "api_style":"completions"}}
        })).unwrap()
    }
    fn fixture_ui<'a>(
        cli: &'a crate::cli::Cli,
        cfg: &'a config::Config,
        session: &'a mut Session,
        context: &'a mut crate::context::ContextFiles,
        workspace: Arc<crate::paths::WorkspaceBinding>,
    ) -> UiContext<'a> {
        UiContext {
            cli,
            cfg,
            session,
            context,
            workspace,
            client: crate::provider::create_client(
                "permission-fixture",
                Some("unused-local-fixture-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox: crate::sandbox::Sandbox::new(false, "none"),
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        }
    }
    fn request(input: &str) -> (AskRequest, oneshot::Receiver<UserDecision>) {
        let (reply, receiver) = oneshot::channel();
        (
            AskRequest {
                tool: "shell".into(),
                input: input.into(),
                #[cfg(feature = "acp")]
                tool_call_id: Some(input.into()),
                suggested_pattern: None,
                additional_allow_patterns: Vec::new(),
                reply,
            },
            receiver,
        )
    }
    fn shown(app: &App<'_>) -> String {
        let lines = app.renderer.feed().line_count(4096);
        app.renderer
            .feed()
            .selected_text(4096, 0, lines.saturating_sub(1))
            .unwrap_or_default()
    }
    async fn sent<T>(sender: &mpsc::Sender<T>, message: T) {
        let result = tokio::time::timeout(Duration::from_secs(2), sender.send(message))
            .await
            .expect("fixture event channel did not accept a message before its deadline");
        assert!(result.is_ok(), "fixture event receiver closed unexpectedly");
    }
    async fn replied(receiver: oneshot::Receiver<UserDecision>) -> UserDecision {
        tokio::time::timeout(Duration::from_secs(2), receiver)
            .await
            .expect("permission decision did not arrive before its deadline")
            .expect("permission decision sender was dropped without a decision")
    }
    async fn drain(app: &mut App<'_>) {
        // Cancel only this borrowed event-loop wait. The App and its owned run
        // remain live for before-answer assertions and the next polling pass.
        match tokio::time::timeout(Duration::from_millis(100), app.run_inner()).await {
            Err(_) => {}
            Ok(Ok(())) => panic!("fixture App exited before the controlled event loop deadline"),
            Ok(Err(error)) => panic!("fixture App event loop failed: {error:#}"),
        }
    }
    async fn runner(app: &mut App<'_>) -> (OwnedRunner, Arc<AtomicBool>, mpsc::Sender<AgentEvent>) {
        let (events, receiver) = mpsc::channel(8);
        let keep_events = events.clone();
        let retired = Arc::new(AtomicBool::new(false));
        let observed = retired.clone();
        let (started, ready) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _retired = Retired(observed);
            let _events = keep_events;
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        let task = OwnedRunner(Some(task));
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .expect("fixture runner did not signal readiness")
            .unwrap();
        app.run.is_running = true;
        app.run.main_abort = Some(task.0.as_ref().unwrap().abort_handle());
        app.run.agent_rx = Some(receiver);
        (task, retired, events)
    }

    #[tokio::test]
    async fn actual_permission_wait_drains_btw_agent_title_logs_and_preserves_paste_before_answer()
    {
        let root = crate::tests::OwnedTestDirectory::new("permission-background-loop");
        let _environment = environment(&root);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = fixture_config();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        session.total_cost = 0.25;
        let mut app = fixture_app(fixture_ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
        ));
        let pending = PendingMainTurn::capture(app.ui.session, "permission-wait prompt");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        let (task, retired, agent_events) = runner(&mut app).await;
        let (asks, receiver) = mpsc::channel(4);
        app.ask_rx = Some(receiver);
        let (ask, mut decision) = request("printf permission-reviewed");
        sent(&asks, ask.into()).await;
        drain(&mut app).await;
        assert!(app.permission_prompt.is_some());
        let id = app
            .btw_pricing
            .start(crate::print::HeadlessPricing::from_session(
                app.ui.session,
                &cfg,
            ))
            .unwrap();
        app.btw_inflight = 1;
        sent(
            &app.btw_tx,
            BtwEvent::Done {
                id,
                response: "BACKGROUND_BTW_BEFORE_ANSWER".into(),
                input_tokens: 2,
                output_tokens: 3,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
        )
        .await;
        sent(
            &app.title_tx,
            SessionTitleResult {
                generation: app.title_generation,
                session_id: app.ui.session.id.clone(),
                title: Ok("BACKGROUND_TITLE".into()),
            },
        )
        .await;
        let (logs, log_receiver) = mpsc::channel(2);
        app.tui_log_rx = Some(log_receiver);
        sent(&logs, "BACKGROUND_LOG_BEFORE_ANSWER".into()).await;
        sent(
            &agent_events,
            AgentEvent::Token("BACKGROUND_AGENT_BEFORE_ANSWER".into()),
        )
        .await;
        sent(&app.user_tx, UserEvent::Paste("DEFERRED_USER_DRAFT".into())).await;
        drain(&mut app).await;
        let transcript = shown(&app);
        let terminal = app.renderer.captured_output();
        for marker in [
            "BACKGROUND_BTW_BEFORE_ANSWER",
            "BACKGROUND_AGENT_BEFORE_ANSWER",
            "BACKGROUND_LOG_BEFORE_ANSWER",
        ] {
            assert!(transcript.contains(marker), "{marker}: {transcript}");
            assert!(
                terminal.contains(marker),
                "background output must be painted before approval: {marker}"
            );
        }
        assert_eq!(
            app.ui.session.generated_title.as_deref(),
            Some("BACKGROUND_TITLE")
        );
        assert!(matches!(
            decision.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(app.renderer.permission_prompt.is_some());
        assert_eq!(
            app.ui.session.total_cost, 0.25,
            "side question accounting never mutates main cost"
        );
        assert_eq!(
            app.input.buffer, "",
            "paste waits until the approval is answered"
        );
        sent(
            &app.user_tx,
            UserEvent::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        )
        .await;
        drain(&mut app).await;
        assert!(matches!(replied(decision).await, UserDecision::Deny));
        assert!(app.run.is_running, "ordinary denial does not stop the run");
        assert!(app.permission_prompt.is_none());
        assert_eq!(app.input.buffer, "DEFERRED_USER_DRAFT");
        drop(agent_events);
        sent(&app.user_tx, UserEvent::Quit).await;
        tokio::time::timeout(Duration::from_secs(2), app.run_inner())
            .await
            .unwrap()
            .unwrap();
        task.joined().await;
        assert!(retired.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn actual_permission_stop_retires_exact_run_denies_backlog_and_stages_next_instruction() {
        let root = crate::tests::OwnedTestDirectory::new("permission-stop-loop");
        let _environment = environment(&root);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = fixture_config();
        for (key, modifiers, progress) in [
            (KeyCode::Esc, KeyModifiers::NONE, false),
            (KeyCode::Char('c'), KeyModifiers::CONTROL, false),
            (KeyCode::Char('d'), KeyModifiers::CONTROL, false),
            (KeyCode::Char('s'), KeyModifiers::NONE, false),
            (KeyCode::Esc, KeyModifiers::NONE, true),
            (KeyCode::Char('s'), KeyModifiers::NONE, true),
        ] {
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
            session.add_message(MessageRole::User, "earlier user");
            session.add_message(MessageRole::Assistant, "earlier answer");
            session.total_cost = 0.25;
            let mut app = fixture_app(fixture_ui(
                &cli,
                &cfg,
                &mut session,
                &mut context,
                workspace,
            ));
            let pending = PendingMainTurn::capture(app.ui.session, "interrupted original prompt");
            crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
            app.run.is_running = true;
            if progress {
                app.ui.session.add_tool_call_with_id(
                    "permission-progress-read",
                    "read",
                    &serde_json::json!({"path":"fixture.txt"}),
                );
                app.ui.session.total_cost = 0.5;
            }
            app.input.load_text("recoverable follow-up draft");
            app.run
                .pending_inputs
                .extend(["stale queued first".into(), "stale queued second".into()]);
            let (task, retired, agent_events) = runner(&mut app).await;
            drop(agent_events);
            let (asks, receiver) = mpsc::channel(4);
            app.ask_rx = Some(receiver);
            let (ask, first) = request("printf current-request");
            sent(&asks, ask.into()).await;
            drain(&mut app).await;
            let (backlog, second) = request("printf stale-request");
            sent(&asks, backlog.into()).await;
            sent(&app.user_tx, UserEvent::Key(KeyEvent::new(key, modifiers))).await;
            drain(&mut app).await;
            assert!(matches!(replied(first).await, UserDecision::Deny));
            assert!(matches!(replied(second).await, UserDecision::Deny));
            task.joined().await;
            assert!(retired.load(Ordering::SeqCst));
            assert!(!app.run.is_running);
            assert!(
                app.run.main_abort.is_none()
                    && app.run.agent_rx.is_none()
                    && app.run.agent.is_none()
            );
            assert!(app.run.pending_inputs.is_empty() && app.run.pending_turn.is_none());
            assert!(app.ui.session.permission_allowlist.is_empty());
            assert_eq!(app.ui.session.total_cost, if progress { 0.5 } else { 0.25 });
            if progress {
                assert!(app.ui.session.messages.iter().any(|message| {
                    message.role == MessageRole::ToolResult
                        && message.tool_call_id.as_deref() == Some("permission-progress-read")
                        && message
                            .content
                            .contains(crate::agent::runner::UNKNOWN_TOOL_OUTCOME)
                }));
            } else {
                assert_eq!(app.ui.session.messages.len(), 2);
            }
            assert_eq!(
                app.input.buffer,
                if progress {
                    "recoverable follow-up draft"
                } else {
                    "interrupted original prompt"
                }
            );
            let transcript = shown(&app);
            assert!(
                transcript.contains("permission denied; run stopped"),
                "{transcript}"
            );
            assert!(
                transcript.contains("dropped 2 queued inputs"),
                "{transcript}"
            );
            // A request sent after the retired generation is never displayed
            // or granted, even if the UI receives a stale allow key afterward.
            if key == KeyCode::Char('s') {
                sent(
                    &app.user_tx,
                    UserEvent::Paste("Read the file instead".into()),
                )
                .await;
                drain(&mut app).await;
                assert!(!shown(&app).contains("> Read the file instead"));
                sent(
                    &app.user_tx,
                    UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                )
                .await;
                drain(&mut app).await;
                assert_eq!(app.input.buffer, "Read the file instead");
                assert!(
                    app.ui
                        .session
                        .messages
                        .iter()
                        .all(|message| message.content != "Read the file instead")
                );
                assert!(!app.run.is_running && app.run.pending_inputs.is_empty());
                app.input.clear_buffer();
                app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
                    .await
                    .unwrap();
                assert_eq!(
                    app.input.buffer,
                    if progress {
                        "recoverable follow-up draft"
                    } else {
                        "interrupted original prompt"
                    }
                );
                if !progress {
                    app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::ALT))
                        .await
                        .unwrap();
                    assert_eq!(app.input.buffer, "recoverable follow-up draft");
                }
            }
            if key != KeyCode::Char('s') && !progress {
                app.input.clear_buffer();
                app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
                    .await
                    .unwrap();
                assert_eq!(app.input.buffer, "recoverable follow-up draft");
            }
            let (stale, stale_reply) = request("printf post-retirement-request");
            sent(&asks, stale.into()).await;
            drain(&mut app).await;
            assert!(matches!(replied(stale_reply).await, UserDecision::Deny));
            assert!(app.permission_prompt.is_none() && !app.run.is_running);
            assert!(app.ui.session.permission_allowlist.is_empty());
        }
    }

    #[tokio::test]
    async fn actual_abandoned_permission_cannot_turn_a_late_allow_into_a_session_grant() {
        let root = crate::tests::OwnedTestDirectory::new("permission-abandoned-loop");
        let _environment = environment(&root);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = fixture_config();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        let mut app = fixture_app(fixture_ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
        ));
        let pending = PendingMainTurn::capture(app.ui.session, "caller-timeout prompt");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        let (task, retired, agent_events) = runner(&mut app).await;
        drop(agent_events);
        let (asks, receiver) = mpsc::channel(4);
        app.ask_rx = Some(receiver);
        let (ask, decision) = request("printf timed-out-request");
        sent(&asks, ask.into()).await;
        drain(&mut app).await;
        assert!(app.permission_prompt.is_some());
        drop(decision);
        sent(
            &app.user_tx,
            UserEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        )
        .await;
        drain(&mut app).await;
        assert!(app.permission_prompt.is_none() && app.renderer.permission_prompt.is_none());
        assert!(app.ui.session.permission_allowlist.is_empty());
        sent(&app.user_tx, UserEvent::Quit).await;
        tokio::time::timeout(Duration::from_secs(2), app.run_inner())
            .await
            .unwrap()
            .unwrap();
        task.joined().await;
        assert!(retired.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn actual_permission_stages_preserve_prompt_folder_and_explicit_session_grants() {
        use crate::permission::checker::{CheckResult, PermissionChecker, PromptGrantOffer};
        let root = crate::tests::OwnedTestDirectory::new("permission-grant-loop");
        let _environment = environment(&root);
        let workspace_path = root.path().join("workspace");
        let external = root.path().join("outside");
        std::fs::create_dir_all(&workspace_path).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let target = external.join("review.txt");
        std::fs::write(&target, "review fixture").unwrap();
        let path = target.to_string_lossy().into_owned();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = fixture_config();
        for kind in ["prompt-accept", "prompt-decline", "folder", "session"] {
            let workspace =
                Arc::new(crate::paths::WorkspaceBinding::capture(&workspace_path).unwrap());
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
            let mut checker = PermissionChecker::new(
                &crate::permission::PermissionConfigs::default(),
                crate::permission::SecurityMode::Standard,
                Some(workspace_path.clone()),
                None,
            )
            .unwrap();
            if kind.starts_with("prompt-") {
                checker.set_prompt_grant_offer(Some(PromptGrantOffer {
                    prompt: "review".into(),
                    dir: external.clone(),
                    tools: vec!["read", "edit"],
                }));
            }
            let permission = Arc::new(std::sync::Mutex::new(checker));
            let mut ui = fixture_ui(&cli, &cfg, &mut session, &mut context, workspace);
            ui.permission = Some(permission.clone());
            let mut app = fixture_app(ui);
            let pending = PendingMainTurn::capture(app.ui.session, "grant review prompt");
            crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
            app.run.is_running = true;
            let (asks, receiver) = mpsc::channel(4);
            app.ask_rx = Some(receiver);
            let (mut ask, mut decision) = request(&path);
            ask.tool = "read".into();
            ask.suggested_pattern = Some(path.clone());
            ask.additional_allow_patterns = vec![format!("{}-specific-second", path)];
            sent(&asks, ask.into()).await;
            drain(&mut app).await;
            if kind == "prompt-decline" {
                assert!(
                    app.renderer
                        .captured_output()
                        .contains("prompt 'review' asks")
                );
                sent(
                    &app.user_tx,
                    UserEvent::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
                )
                .await;
                drain(&mut app).await;
                assert!(matches!(
                    decision.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));
                assert!(
                    app.permission_prompt.is_some(),
                    "declining the broad prompt grant falls back to per-call approval"
                );
                assert!(app.ui.session.permission_allowlist.is_empty());
                assert!(
                    permission
                        .lock()
                        .unwrap()
                        .prompt_grant_offer_for("read", &path)
                        .is_none()
                );
            }
            let answer = match kind {
                "folder" => 'f',
                "session" => 'a',
                _ => 'y',
            };
            sent(
                &app.user_tx,
                UserEvent::Key(KeyEvent::new(KeyCode::Char(answer), KeyModifiers::NONE)),
            )
            .await;
            drain(&mut app).await;
            match (kind, replied(decision).await) {
                ("session", UserDecision::AllowAlways(pattern)) => {
                    assert_eq!(pattern, path);
                    assert_eq!(app.ui.session.permission_allowlist.len(), 2);
                    assert!(
                        app.ui
                            .session
                            .permission_allowlist
                            .iter()
                            .all(|entry| entry.tool == "read")
                    );
                }
                ("prompt-decline", UserDecision::AllowOnce) => {
                    assert!(app.ui.session.permission_allowlist.is_empty());
                    assert_eq!(
                        permission.lock().unwrap().check_path("edit", &path),
                        CheckResult::Ask
                    );
                }
                ("prompt-accept" | "folder", UserDecision::AllowOnce) => {
                    assert!(!app.ui.session.permission_allowlist.is_empty());
                    assert_eq!(
                        permission.lock().unwrap().check_path("edit", &path),
                        CheckResult::Allowed
                    );
                    assert_eq!(
                        permission.lock().unwrap().check_path("write", &path),
                        CheckResult::Ask
                    );
                }
                (_, other) => panic!("unexpected grant for {kind}: {other:?}"),
            }
            assert!(app.permission_prompt.is_none() && app.run.is_running);
            sent(&app.user_tx, UserEvent::Quit).await;
            tokio::time::timeout(Duration::from_secs(2), app.run_inner())
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn actual_mode_cycle_preserves_drafts_queue_and_unanswered_permission() {
        use crate::permission::{SecurityMode, checker::PermissionChecker};
        let root = crate::tests::OwnedTestDirectory::new("mode-cycle-app");
        let _environment = environment(&root);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = fixture_config();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        let permission = Arc::new(std::sync::Mutex::new(
            PermissionChecker::new(
                &crate::permission::PermissionConfigs::default(),
                SecurityMode::Standard,
                Some(root.path().into()),
                None,
            )
            .unwrap(),
        ));
        let mut ui = fixture_ui(&cli, &cfg, &mut session, &mut context, workspace);
        ui.permission = Some(permission.clone());
        let mut app = fixture_app(ui);
        app.input.buffer = "typed follow-up".into();
        app.input.cursor = app.input.buffer.len();
        let cycle = || UserEvent::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        for mode in [
            SecurityMode::Guarded,
            SecurityMode::Yolo,
            SecurityMode::Standard,
        ] {
            let _ = app.handle_user_event(cycle()).await.unwrap();
            assert_eq!(permission.lock().unwrap().mode(), mode);
            assert_eq!(
                app.renderer.notice_text(),
                Some(format!("security mode: {mode}").as_str())
            );
            assert!(
                app.renderer
                    .captured_output()
                    .contains(&format!("mode:{mode}"))
            );
        }
        let mut empty_picker = crate::ui::pickers::list::ListPicker::new();
        empty_picker.activate();
        app.input.picker = Some(crate::ui::input::Picker::Bang(empty_picker));
        let _ = app.handle_user_event(cycle()).await.unwrap();
        assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Standard);
        app.input.picker = None;
        app.input.start_command_picker();
        if let Some(crate::ui::input::Picker::Command(picker)) = app.input.picker.as_mut() {
            picker.selected = 1;
        }
        app.handle_key_event(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await
            .unwrap();
        if let Some(crate::ui::input::Picker::Command(picker)) = app.input.picker.as_ref() {
            assert_eq!(picker.selected, 0);
        } else {
            panic!("picker was closed by mode shortcut");
        }
        assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Standard);
        app.input.picker = None;
        let pending = PendingMainTurn::capture(app.ui.session, "original active prompt");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        app.run.pending_inputs.push_back("queued follow-up".into());
        let (ask, mut decision) = request("pending exact shell request");
        app.begin_permission_request(ask).unwrap();
        let _ = app.handle_user_event(cycle()).await.unwrap();
        assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Guarded);
        assert!(matches!(
            decision.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(app.permission_prompt.is_some());
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert_eq!(app.input.buffer, "typed follow-up");
        assert_eq!(app.run.pending_inputs.len(), 1);
        let modified = UserEvent::Key(KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        let _ = app.handle_user_event(modified).await.unwrap();
        assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Guarded);
        permission
            .lock()
            .unwrap()
            .set_prompt_mode(SecurityMode::ReadOnly);
        permission.lock().unwrap().restore_user_mode();
        assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Guarded);
        let _ = app
            .handle_user_event(UserEvent::Key(KeyEvent::new(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
            )))
            .await
            .unwrap();
        assert!(matches!(replied(decision).await, UserDecision::Deny));
    }

    #[tokio::test]
    async fn actual_mode_cycle_uses_custom_order_and_enters_ring_from_other_modes() {
        use crate::permission::{SecurityMode, checker::PermissionChecker};
        let root = crate::tests::OwnedTestDirectory::new("mode-cycle-custom-app");
        let _environment = environment(&root);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        for (ring, current, expected) in [
            (None, SecurityMode::ReadOnly, vec![SecurityMode::Standard]),
            (
                Some(vec!["readonly", "planwrite", "guarded"]),
                SecurityMode::Standard,
                vec![
                    SecurityMode::ReadOnly,
                    SecurityMode::PlanWrite,
                    SecurityMode::Guarded,
                    SecurityMode::ReadOnly,
                ],
            ),
            (
                Some(vec!["readonly"]),
                SecurityMode::Guarded,
                vec![SecurityMode::ReadOnly, SecurityMode::ReadOnly],
            ),
        ] {
            let mut cfg = fixture_config();
            cfg.permission_mode_cycle =
                ring.map(|ring| ring.into_iter().map(str::to_owned).collect());
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
            let permission = Arc::new(std::sync::Mutex::new(
                PermissionChecker::new(
                    &crate::permission::PermissionConfigs::default(),
                    current,
                    Some(root.path().into()),
                    None,
                )
                .unwrap(),
            ));
            let mut ui = fixture_ui(&cli, &cfg, &mut session, &mut context, workspace);
            ui.permission = Some(permission.clone());
            let mut app = fixture_app(ui);
            for mode in expected {
                let _ = app
                    .handle_user_event(UserEvent::Key(KeyEvent::new(
                        KeyCode::BackTab,
                        KeyModifiers::NONE,
                    )))
                    .await
                    .unwrap();
                assert_eq!(permission.lock().unwrap().mode(), mode);
                permission.lock().unwrap().restore_user_mode();
                assert_eq!(permission.lock().unwrap().mode(), mode);
                assert_eq!(
                    app.renderer.notice_text(),
                    Some(format!("security mode: {mode}").as_str())
                );
            }
        }
    }

    #[tokio::test]
    async fn actual_workspace_edit_answer_grants_only_native_mutations_and_rejects_stale_requests()
    {
        use crate::permission::{
            SecurityMode,
            checker::{CheckResult, PermissionChecker, PromptGrantOffer},
        };
        let root = crate::tests::OwnedTestDirectory::new("workspace-edit-answer-app");
        let _environment = environment(&root);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = fixture_config();
        for kind in [
            "write",
            "edit",
            "read",
            "external",
            "stale",
            "retired",
            "prompt-offer",
        ] {
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
            let target = workspace
                .root()
                .join("new.txt")
                .to_string_lossy()
                .into_owned();
            let mut checker = PermissionChecker::new(
                &crate::permission::PermissionConfigs::default(),
                SecurityMode::Restrictive,
                Some(root.path().into()),
                None,
            )
            .unwrap();
            if kind == "prompt-offer" {
                checker.set_prompt_grant_offer(Some(PromptGrantOffer {
                    prompt: "review".into(),
                    dir: workspace.root().into(),
                    tools: vec!["read", "edit"],
                }));
            }
            let permission = Arc::new(std::sync::Mutex::new(checker));
            let mut ui = fixture_ui(&cli, &cfg, &mut session, &mut context, workspace);
            ui.permission = Some(permission.clone());
            let mut app = fixture_app(ui);
            let pending = PendingMainTurn::capture(app.ui.session, "grant mutation request");
            crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
            app.run.is_running = true;
            let outside = root
                .path()
                .parent()
                .unwrap()
                .join("ungranted-neighbor.txt")
                .to_string_lossy()
                .into_owned();
            let (mut ask, mut decision) = request(if kind == "external" {
                &outside
            } else {
                &target
            });
            ask.tool = if kind == "read" {
                "read"
            } else if kind == "write" {
                "write"
            } else {
                "edit"
            }
            .into();
            let (asks, receiver) = mpsc::channel(4);
            app.ask_rx = Some(receiver);
            sent(&asks, ask.into()).await;
            drain(&mut app).await;
            let eligible = !matches!(kind, "read" | "external");
            assert_eq!(
                app.renderer
                    .captured_output()
                    .contains("(w) allow writes/edits"),
                eligible
            );
            if kind == "stale" {
                drop(decision);
                let _ = app
                    .handle_user_event(UserEvent::Key(KeyEvent::new(
                        KeyCode::Char('w'),
                        KeyModifiers::NONE,
                    )))
                    .await
                    .unwrap();
                assert!(app.ui.session.permission_allowlist.is_empty());
                continue;
            }
            if kind == "retired" {
                app.run.is_running = false;
            }
            sent(
                &app.user_tx,
                UserEvent::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
            )
            .await;
            drain(&mut app).await;
            if !eligible {
                assert!(matches!(
                    decision.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));
                assert!(app.ui.session.permission_allowlist.is_empty());
                let _ = app
                    .handle_user_event(UserEvent::Key(KeyEvent::new(
                        KeyCode::Char('n'),
                        KeyModifiers::NONE,
                    )))
                    .await
                    .unwrap();
                assert!(matches!(replied(decision).await, UserDecision::Deny));
            } else if kind == "retired" {
                assert!(matches!(replied(decision).await, UserDecision::Deny));
                assert!(app.ui.session.permission_allowlist.is_empty());
            } else {
                assert!(matches!(replied(decision).await, UserDecision::AllowOnce));
                assert_eq!(app.ui.session.permission_allowlist.len(), 4);
                assert!(
                    app.ui
                        .session
                        .permission_allowlist
                        .iter()
                        .all(|entry| matches!(entry.tool.as_str(), "write" | "edit"))
                );
                for tool in ["write", "edit"] {
                    assert_eq!(
                        permission.lock().unwrap().check_path(tool, &target),
                        CheckResult::Allowed
                    );
                }
                for tool in ["read", "js/write_file"] {
                    assert_eq!(
                        permission.lock().unwrap().check_path(tool, &target),
                        CheckResult::Ask
                    );
                }
                assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Restrictive);
                if kind == "prompt-offer" {
                    assert!(
                        permission
                            .lock()
                            .unwrap()
                            .prompt_grant_offer_for("read", &target)
                            .is_some()
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actual_workspace_edit_answer_rejects_binding_replaced_while_prompt_waits() {
        use crate::permission::{
            SecurityMode,
            checker::{CheckResult, PermissionChecker},
        };
        let root = crate::tests::OwnedTestDirectory::new("workspace-edit-replaced-app");
        let _environment = environment(&root);
        let directory = root.path().join("workspace");
        std::fs::create_dir(&directory).unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&directory).unwrap());
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = fixture_config();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        let permission = Arc::new(std::sync::Mutex::new(
            PermissionChecker::new(
                &crate::permission::PermissionConfigs::default(),
                SecurityMode::Restrictive,
                Some(directory.clone()),
                None,
            )
            .unwrap(),
        ));
        let target = workspace
            .root()
            .join("new.txt")
            .to_string_lossy()
            .into_owned();
        let mut ui = fixture_ui(&cli, &cfg, &mut session, &mut context, workspace);
        ui.permission = Some(permission.clone());
        let mut app = fixture_app(ui);
        let pending = PendingMainTurn::capture(app.ui.session, "approve retained workspace");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        let (mut ask, decision) = request(&target);
        ask.tool = "write".into();
        app.begin_permission_request(ask).unwrap();
        assert!(
            app.renderer
                .captured_output()
                .contains("(w) allow writes/edits")
        );
        std::fs::rename(&directory, root.path().join("retained")).unwrap();
        std::fs::create_dir(&directory).unwrap();
        let _ = app
            .handle_user_event(UserEvent::Key(KeyEvent::new(
                KeyCode::Char('w'),
                KeyModifiers::NONE,
            )))
            .await
            .unwrap();
        assert!(matches!(replied(decision).await, UserDecision::Deny));
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert_eq!(
            permission.lock().unwrap().check_path("write", &target),
            CheckResult::Ask
        );
        assert!(!directory.join("new.txt").exists());
        assert!(app.permission_prompt.is_none());
    }

    #[tokio::test]
    async fn actual_terminal_error_denies_displayed_permission_and_backlog_before_settlement() {
        let root = crate::tests::OwnedTestDirectory::new("permission-error-loop");
        let _environment = environment(&root);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = fixture_config();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        let mut app = fixture_app(fixture_ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
        ));
        let pending = PendingMainTurn::capture(app.ui.session, "failed permission prompt");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        app.run.pending_inputs.push_back("stale error queue".into());
        let (asks, receiver) = mpsc::channel(4);
        app.ask_rx = Some(receiver);
        let (first, first_reply) = request("printf error-first");
        sent(&asks, first.into()).await;
        drain(&mut app).await;
        let (second, second_reply) = request("printf error-second");
        sent(&asks, second.into()).await;
        let (events, receiver) = mpsc::channel(2);
        app.run.agent_rx = Some(receiver);
        sent(
            &events,
            AgentEvent::Error {
                message: "permission owner failed".into(),
                interactions: Vec::new(),
            },
        )
        .await;
        drop(events); // Real terminal event is followed by owned runner closure.
        drain(&mut app).await;
        assert!(matches!(replied(first_reply).await, UserDecision::Deny));
        assert!(matches!(replied(second_reply).await, UserDecision::Deny));
        assert!(app.permission_prompt.is_none() && app.renderer.permission_prompt.is_none());
        assert!(
            !app.run.is_running && app.run.agent_rx.is_none() && app.run.pending_turn.is_none()
        );
        assert!(
            app.run.pending_inputs.is_empty() && app.ui.session.permission_allowlist.is_empty()
        );
        assert_eq!(app.input.buffer, "failed permission prompt");
        assert!(shown(&app).contains("permission owner failed"));
    }

    #[tokio::test(start_paused = true)]
    async fn actual_permission_stop_timeout_retains_cleanup_and_blocks_next_instruction() {
        let root = crate::tests::OwnedTestDirectory::new("permission-retirement-timeout");
        let _environment = environment(&root);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = fixture_config();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        let mut app = fixture_app(fixture_ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
        ));
        let pending = PendingMainTurn::capture(app.ui.session, "unsettled original prompt");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        app.run
            .pending_inputs
            .push_back("must not restart unsettled work".into());
        let (task, retired, retained_events) = runner(&mut app).await;
        let (asks, receiver) = mpsc::channel(4);
        app.ask_rx = Some(receiver);
        let (ask, decision) = request("printf unsettled-request");
        sent(&asks, ask.into()).await;
        drain(&mut app).await;
        assert!(app.permission_prompt.is_some());
        sent(
            &app.user_tx,
            UserEvent::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
        )
        .await;
        // The test holds one exact-run sender beyond cancellation. Advancing
        // Tokio time deterministically triggers the real retirement timeout.
        let error = tokio::time::timeout(Duration::from_secs(20), app.run())
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("permission stop cleanup is still pending")
        );
        assert!(matches!(replied(decision).await, UserDecision::Deny));
        assert!(
            app.run.agent_rx.is_some(),
            "teardown must retain the exact unsettled receiver"
        );
        assert!(app.run.pending_inputs.is_empty() && app.run.pending_turn.is_none());
        assert!(!app.run.is_running && app.run.agent.is_none());
        assert!(app.permission_prompt.is_none() && app.renderer.permission_prompt.is_none());
        assert_eq!(app.input.buffer, "unsettled original prompt");
        assert!(
            !app.renderer
                .captured_output()
                .contains("Next user instruction (draft)")
        );
        task.joined().await;
        assert!(retired.load(Ordering::SeqCst));
        drop(retained_events); // Release the controlled cleanup obstruction.
        tokio::time::timeout(Duration::from_secs(2), app.teardown())
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod mutation_consumer_tests {
    use super::test_support::fixture_app;
    use super::*;
    use crate::agent::tools::mutation_diff::MutationDiff;
    use crate::agent::tools::{ReadArgs, ReadTool, ReadTracker, WriteArgs, WriteTool};
    use rig::tool::{Tool, ToolCallExtensions};

    #[tokio::test]
    async fn actual_app_channel_renders_only_matching_committed_diff_and_replays_same_preview() {
        let root = crate::tests::OwnedTestDirectory::new("mutation-app");
        let bindings = [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ]
        .map(|name| (name, Some(root.path().join(name).into_os_string())));
        let _env = crate::tests::ScopedProcessEnv::set(&bindings);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("openrouter", "test-model", 128000, "");
        let ui = UiContext {
            cli: &cli,
            cfg: &cfg,
            session: &mut session,
            context: &mut context,
            workspace: workspace.clone(),
            client: crate::provider::create_client(
                "openrouter",
                Some("unused-test-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox: crate::sandbox::Sandbox::new(false, "none"),
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        let mut app = fixture_app(ui);
        std::fs::write(root.path().join("target.txt"), "old🙂\n").unwrap();
        let tracker = ReadTracker::new(true);
        ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
            .with_workspace(root.path())
            .call(ReadArgs {
                raw_notebook: false,
                path: "target.txt".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        let result = WriteTool::new_with_tracker(None, None, None, tracker)
            .with_workspace(root.path())
            .call_structured(
                WriteArgs {
                    path: "target.txt".into(),
                    content: "newα\x1b]52;c;ZXZpbA==\x07\n".into(),
                    overwrite: true,
                },
                &ToolCallExtensions::new(),
            )
            .await
            .unwrap();
        let diff = result.extensions.get::<MutationDiff>().unwrap().clone();
        let (sender, mut receiver) = mpsc::channel(8);
        for event in [
            AgentEvent::ToolCall {
                id: "actual".into(),
                name: "write".into(),
                args: serde_json::json!({"path":"target.txt"}),
            },
            AgentEvent::FileMutation {
                id: "wrong".into(),
                name: "write".into(),
                diff: diff.clone(),
            },
            AgentEvent::FileMutation {
                id: "actual".into(),
                name: "edit".into(),
                diff: diff.clone(),
            },
            AgentEvent::FileMutation {
                id: "actual".into(),
                name: "write".into(),
                diff: diff.clone(),
            },
            AgentEvent::ToolResult {
                id: "actual".into(),
                name: "write".into(),
                output: result.output.as_str().into(),
                is_error: false,
            },
        ] {
            sender.send(event).await.unwrap();
        }
        drop(sender);
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
                .await
                .unwrap()
        {
            app.handle_agent_event(event).await.unwrap();
        }
        assert_eq!(app.ui.session.messages.len(), 2);
        assert_eq!(
            app.ui.session.messages[0].recorded_diff.as_ref(),
            Some(&diff)
        );
        assert!(app.ui.session.messages[1].recorded_diff.is_none());
        let live = app
            .renderer
            .feed()
            .selected_text(
                180,
                0,
                app.renderer.feed().line_count(180).saturating_sub(1),
            )
            .unwrap();
        assert_eq!(live.matches("+newα").count(), 1);
        assert_eq!(live.matches("-old🙂").count(), 1);
        let painted = app.renderer.captured_output();
        assert!(painted.contains("+newα") && painted.contains("-old🙂"));
        assert!(!painted.contains("]52;") && !painted.contains("ZXZpbA=="));
        let mut replay = crate::ui::feed::Feed::new();
        crate::ui::events::push_session_messages(&mut replay, &app.ui.session.messages, &cfg);
        let replayed = replay
            .selected_text(180, 0, replay.line_count(180).saturating_sub(1))
            .unwrap();
        assert!(replayed.contains("+newα") && replayed.contains("-old🙂"));
        let off: crate::config::Config =
            serde_json::from_value(serde_json::json!({"show_tool_details":false})).unwrap();
        let mut hidden = crate::ui::feed::Feed::new();
        crate::ui::events::push_session_messages(&mut hidden, &app.ui.session.messages, &off);
        let hidden = hidden
            .selected_text(180, 0, hidden.line_count(180).saturating_sub(1))
            .unwrap();
        assert!(
            hidden.contains("write") && !hidden.contains("+newα") && !hidden.contains("-old🙂")
        );
    }
}

#[cfg(test)]
mod compaction_visibility_tests {
    use super::test_support::fixture_app;
    use super::*;

    #[tokio::test]
    async fn actual_compact_show_is_read_only_and_sanitizes_the_active_saved_summary() {
        let root = crate::tests::OwnedTestDirectory::new("compact-show-app");
        let bindings = [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ]
        .map(|name| (name, Some(root.path().join(name).into_os_string())));
        let _env = crate::tests::ScopedProcessEnv::set(&bindings);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg:crate::config::Config=serde_json::from_value(serde_json::json!({"custom_providers":{"local-test":{"provider_type":"openai","base_url":"http://127.0.0.1:9/v1","api_style":"completions"}}})).unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session =
            Session::new("local-test", "test", 128_000, root.path().to_str().unwrap());
        session.add_message(crate::session::MessageRole::User, "old turn");
        session.compress(
            "## Task\nSHOW_THIS_SUMMARY\x1b]52;c;ZXZpbA==\x07\n## Next Step\ncontinue".into(),
            1,
            10,
        );
        let before = serde_json::to_vec(&session).unwrap();
        let ui = UiContext {
            cli: &cli,
            cfg: &cfg,
            session: &mut session,
            context: &mut context,
            workspace: workspace.clone(),
            client: crate::provider::create_client(
                "local-test",
                Some("unused-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox: crate::sandbox::Sandbox::new(false, "none"),
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        let mut app = fixture_app(ui);
        app.run_slash_command("/compact show").await.unwrap();
        let text = app.renderer.captured_output();
        assert!(text.contains("SHOW_THIS_SUMMARY"));
        assert!(text.contains("previous summary"));
        assert!(
            !text.contains("\x1b]52"),
            "saved content must not emit a terminal command"
        );
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert!(
            app.run.main_abort.is_none(),
            "show must not start a provider turn"
        );
        app.run_slash_command("/compact show extra").await.unwrap();
        assert!(
            app.renderer
                .captured_output()
                .contains("usage: /compact show")
        );
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        crate::ui::events::render_session(
            &mut app.renderer,
            app.ui.session,
            &cli,
            &cfg,
            app.ui.context,
        )
        .unwrap();
        assert!(app.renderer.captured_output().contains("/compact show"));

        // Imported/oversized recaps remain stored exactly; display is bounded
        // and the actual command must not mutate or dispatch a provider turn.
        app.ui.session.compress("é".repeat(40_000), 1, 1);
        app.ui
            .session
            .compactions
            .last_mut()
            .unwrap()
            .file_requests
            .mark_imported();
        let large = serde_json::to_vec(app.ui.session).unwrap();
        app.run_slash_command("/compact show").await.unwrap();
        let text = app.renderer.captured_output();
        assert!(text.contains("summary display truncated; saved recap unchanged"));
        assert!(text.contains("imported, untrusted historical data"));
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), large);
        assert!(app.run.main_abort.is_none());

        *app.ui.session = Session::new("local-test", "test", 128_000, "");
        let empty = serde_json::to_vec(app.ui.session).unwrap();
        app.run_slash_command("/compact show").await.unwrap();
        assert!(
            app.renderer
                .captured_output()
                .contains("no active compaction summary")
        );
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), empty);
        assert!(app.run.main_abort.is_none());
    }
}

#[cfg(test)]
mod custom_command_tests {
    use super::test_support::fixture_app;
    use super::*;

    #[tokio::test]
    async fn actual_custom_slash_submits_one_literal_user_turn_and_restores_one_shot_context() {
        let root = crate::tests::OwnedTestDirectory::new("custom-slash-app");
        let bindings = [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ]
        .map(|name| (name, Some(root.path().join(name).into_os_string())));
        let _environment = crate::tests::ScopedProcessEnv::set(&bindings);
        let cli = crate::cli::Cli {
            no_session: true,
            api_key: Some("unused-key".into()),
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({"custom_providers":{"local-test":{"provider_type":"openai","base_url":"http://127.0.0.1:9/v1","api_style":"completions"}},"quick_models":{"same":{"provider":"local-test","model":"chosen-model","input_token_cost":2.0,"output_token_cost":4.0}},"retry":{"max_attempts":1}})).unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        context
            .prompts
            .insert("plan".into(), "ONE_SHOT_PLAN".into());
        let original = context.active_selection();
        let paths = crate::paths::process_paths().unwrap();
        std::fs::create_dir_all(paths.config_dir.join("commands")).unwrap();
        std::fs::write(paths.config_dir.join("commands/fix-issue.md"), "---\ndescription: Fix an issue\nmode: plan\nmodel: same\n---\n%%mode=readonly\n/clear !touch injected $1\n$ARGUMENTS").unwrap();
        std::fs::write(
            paths.config_dir.join("commands/blocked.md"),
            "---\nmode: plan\nmodel: same\n---\n@missing",
        )
        .unwrap();
        let mut session = Session::new(
            "local-test",
            "test-model",
            128_000,
            root.path().to_str().unwrap(),
        );
        let ui = UiContext {
            cli: &cli,
            cfg: &cfg,
            session: &mut session,
            context: &mut context,
            workspace,
            client: crate::provider::create_client(
                "local-test",
                Some("unused-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox: crate::sandbox::Sandbox::new(false, "none"),
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        let mut app = fixture_app(ui);
        let before = serde_json::to_vec(app.ui.session).unwrap();
        app.run_slash_command("/blocked").await.unwrap();
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert_eq!(app.ui.context.active_selection(), original);
        assert!(app.run.main_abort.is_none());
        std::fs::write(root.path().join("missing"), "PRIVATE_DENIED_CONTENT").unwrap();
        for action in [
            crate::permission::Action::Deny,
            crate::permission::Action::Ask,
        ] {
            let policy =
                crate::permission::PermissionConfigs::from(crate::permission::PermissionConfig {
                    read: Some(crate::permission::ToolPerm::Simple(action)),
                    ..Default::default()
                });
            let checker = crate::permission::checker::PermissionChecker::new(
                &policy,
                crate::permission::SecurityMode::Standard,
                Some(root.path().to_path_buf()),
                None,
            )
            .unwrap();
            app.ui.permission = Some(Arc::new(std::sync::Mutex::new(checker)));
            app.run_slash_command("/blocked").await.unwrap();
            assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
            assert_eq!(app.ui.context.active_selection(), original);
            assert_eq!(
                app.ui.permission.as_ref().unwrap().lock().unwrap().mode(),
                crate::permission::SecurityMode::Standard
            );
            assert!(app.run.main_abort.is_none());
            assert!(
                !app.renderer
                    .captured_output()
                    .contains("PRIVATE_DENIED_CONTENT")
            );
        }
        app.run_slash_command("/help").await.unwrap();
        assert!(
            app.renderer
                .captured_output()
                .contains("/fix-issue [arguments]  Fix an issue")
        );
        app.renderer.clear_content().unwrap();
        app.run_slash_command("/fix-issue 123 @missing %%mode=yolo")
            .await
            .unwrap();
        let assertions = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
            assert_eq!(app.chain.dot_prompt_restore, Some(original.clone()));
            let users: Vec<_> = app
                .ui
                .session
                .messages
                .iter()
                .filter(|message| message.role == crate::session::MessageRole::User)
                .collect();
            assert_eq!(users.len(), 1);
            assert_eq!(
                users[0].content,
                "/clear !touch injected 123\n123 @missing %%mode=yolo"
            );
            let feed = app.renderer.feed();
            let transcript = (0..feed.block_count())
                .filter_map(|index| feed.block_text(index))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                transcript.matches("> /clear !touch injected 123").count(),
                1
            );
            assert!(!root.path().join("injected").exists());
            assert_eq!(app.ui.session.model, "chosen-model");
            assert_eq!(app.ui.session.input_token_cost, 2.0);
            assert_eq!(
                app.ui.permission.as_ref().unwrap().lock().unwrap().mode(),
                crate::permission::SecurityMode::ReadOnly
            );
        }));
        // Settle the owned runner on every assertion outcome before fixture
        // teardown: no real provider is used; endpoint is loopback discard.
        app.run
            .retire(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        if let Err(panic) = assertions {
            std::panic::resume_unwind(panic);
        }
        app.run.is_running = false;
        app.finalize_turn().await.unwrap();
        assert_eq!(app.ui.context.active_selection(), original);
        assert_eq!(
            app.ui.permission.as_ref().unwrap().lock().unwrap().mode(),
            crate::permission::SecurityMode::Standard
        );
        assert_eq!(
            app.ui.session.model, "chosen-model",
            "quick-model switches persist as with existing dot prompts"
        );
    }
}

#[cfg(test)]
mod workspace_session_picker_tests {
    use super::test_support::fixture_app;
    use super::*;
    fn environment(root: &crate::tests::OwnedTestDirectory) -> crate::tests::ScopedProcessEnv {
        crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ])
    }
    fn ui<'a>(
        cli: &'a crate::cli::Cli,
        cfg: &'a config::Config,
        session: &'a mut Session,
        context: &'a mut crate::context::ContextFiles,
        workspace: Arc<crate::paths::WorkspaceBinding>,
    ) -> UiContext<'a> {
        UiContext::new(
            cli,
            cfg,
            session,
            context,
            workspace,
            crate::provider::create_client(
                "anthropic",
                Some("unused-local-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            None,
            None,
            crate::sandbox::Sandbox::new(false, "none"),
            None,
        )
    }
    fn store(session: &Session) {
        crate::session::storage::atomic_write(
            &crate::session::storage::session_path(&session.id).unwrap(),
            &serde_json::to_string(session).unwrap(),
        )
        .unwrap();
    }
    #[tokio::test]
    async fn actual_app_picker_scope_toggle_and_exact_tool_history_resume_use_owned_renderer() {
        let root = crate::tests::OwnedTestDirectory::new("app-session-picker");
        let _env = environment(&root);
        let foreign = root.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let cfg = config::Config {
            session_title_model: Some("off".into()),
            ..Default::default()
        };
        let mut current = Session::new("anthropic", "test", 128_000, "");
        current.working_dir = root.path().to_string_lossy().into();
        current.add_message(crate::session::MessageRole::User, "CURRENT_PICKER_ROW");
        current.add_tool_call_with_id(
            "captured-call",
            "read",
            &serde_json::json!({"path":"a.txt"}),
        );
        current.add_tool_result_with_id("captured-call", "read", "CAPTURED_TOOL_BODY");
        current.permission_allowlist = vec![crate::session::PermissionAllowEntry {
            tool: "bash".into(),
            pattern: "incoming-command".into(),
        }];
        store(&current);
        let mut other = Session::new("anthropic", "test", 128_000, "");
        other.working_dir = foreign.to_string_lossy().into();
        other.add_message(crate::session::MessageRole::User, "FOREIGN_PICKER_ROW");
        other.permission_allowlist = vec![crate::session::PermissionAllowEntry {
            tool: "bash".into(),
            pattern: "*".into(),
        }];
        store(&other);
        let mut session = Session::new("anthropic", "test", 128_000, "");
        session.working_dir = root.path().to_string_lossy().into();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut app = fixture_app(ui(&cli, &cfg, &mut session, &mut context, workspace));
        let mut checker = crate::permission::checker::PermissionChecker::new(
            &crate::permission::PermissionConfigs::default(),
            crate::permission::SecurityMode::Standard,
            Some(root.path().to_owned()),
            None,
        )
        .unwrap();
        checker.add_session_allowlist("bash".into(), "outgoing-command");
        app.ui.permission = Some(Arc::new(std::sync::Mutex::new(checker)));
        app.run_slash_command("/sessions").await.unwrap();
        app.refresh().unwrap();
        let rendered = app.renderer.captured_output();
        assert!(rendered.contains("Current workspace"));
        assert!(rendered.contains("CURRENT_PICKER_ROW"));
        assert!(!rendered.contains("FOREIGN_PICKER_ROW"));
        assert!(matches!(
            app.handle_user_event(UserEvent::Key(KeyEvent::new(
                KeyCode::Tab,
                KeyModifiers::NONE,
            )))
            .await
            .unwrap(),
            ControlFlow::Continue(())
        ));
        let rendered = app.renderer.captured_output();
        assert!(rendered.contains("All workspaces"));
        assert!(rendered.contains("FOREIGN_PICKER_ROW"));
        assert!(matches!(
            app.handle_user_event(UserEvent::Paste("CURRENT_PICKER_ROW".into()))
                .await
                .unwrap(),
            ControlFlow::Continue(())
        ));
        assert!(matches!(
            app.handle_user_event(UserEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .await
            .unwrap(),
            ControlFlow::Continue(())
        ));
        assert_eq!(app.ui.session.id, current.id);
        assert_eq!(app.ui.session.messages.len(), 3);
        assert_eq!(
            app.ui.session.messages[2].content,
            current.messages[2].content
        );
        assert_eq!(
            app.ui.session.messages[2].tool_call_id.as_deref(),
            Some("captured-call")
        );
        assert!(app.input.picker.is_none());
        {
            let mut permission = app.ui.permission.as_ref().unwrap().lock().unwrap();
            assert_eq!(
                permission.check("bash", "incoming-command"),
                crate::permission::checker::CheckResult::Allowed
            );
            assert_eq!(
                permission.check("bash", "outgoing-command"),
                crate::permission::checker::CheckResult::Ask
            );
        }
        app.run_slash_command("/sessions --all").await.unwrap();
        assert!(matches!(
            app.handle_user_event(UserEvent::Paste("FOREIGN_PICKER_ROW".into()))
                .await
                .unwrap(),
            ControlFlow::Continue(())
        ));
        assert!(matches!(
            app.handle_user_event(UserEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .await
            .unwrap(),
            ControlFlow::Continue(())
        ));
        assert_ne!(app.ui.session.id, other.id);
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert_eq!(app.ui.session.working_dir, current.working_dir);
        assert_eq!(
            app.ui
                .permission
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .check("bash", "incoming-command"),
            crate::permission::checker::CheckResult::Ask
        );
        let foreign_fork = app.ui.session.id.clone();
        app.run_slash_command("/sessions").await.unwrap();
        assert!(matches!(
            app.handle_user_event(UserEvent::Paste("CURRENT_PICKER_ROW".into()))
                .await
                .unwrap(),
            ControlFlow::Continue(())
        ));
        app.run.is_running = true;
        assert!(matches!(
            app.handle_user_event(UserEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .await
            .unwrap(),
            ControlFlow::Continue(())
        ));
        assert_eq!(
            app.ui.session.id, foreign_fork,
            "busy run cannot publish a selected successor"
        );
    }
    #[test]
    fn actual_session_picker_event_loop_runs_on_one_mib_stack() {
        std::thread::Builder::new()
            .name("session-picker-one-mib".into())
            .stack_size(1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(crate::startup::heap_future(|| async {
                        let root = crate::tests::OwnedTestDirectory::new("picker-one-mib");
                        let _env = environment(&root);
                        let workspace =
                            Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
                        let cli = crate::cli::Cli {
                            no_session: true,
                            tools: vec!["read".into()],
                            ..Default::default()
                        };
                        let cfg = config::Config {
                            session_title_model: Some("off".into()),
                            ..Default::default()
                        };
                        let mut saved = Session::new("anthropic", "test", 128_000, "");
                        saved.working_dir = root.path().to_string_lossy().into();
                        saved.add_message(crate::session::MessageRole::User, "ONE_MIB_SAVED_ROW");
                        store(&saved);
                        let mut session = Session::new("anthropic", "test", 128_000, "");
                        session.working_dir = root.path().to_string_lossy().into();
                        let mut context =
                            crate::context::load(true).for_workspace_binding(true, &workspace);
                        let mut app =
                            fixture_app(ui(&cli, &cfg, &mut session, &mut context, workspace));
                        let catalogue =
                            crate::session::catalogue::list(&app.ui.workspace, false).unwrap();
                        app.input
                            .start_session_picker(catalogue, app.ui.workspace.clone(), false);
                        app.deferred_user_events.extend([
                            UserEvent::Paste("ONE_MIB_SAVED_ROW".into()),
                            UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                            UserEvent::Quit,
                        ]);
                        tokio::time::timeout(
                            Duration::from_secs(3),
                            crate::startup::heap_future(|| app.run_inner()),
                        )
                        .await
                        .unwrap()
                        .unwrap();
                        assert_eq!(app.ui.session.id, saved.id);
                        assert!(app.renderer.captured_output().contains("ONE_MIB_SAVED_ROW"));
                        assert!(app.input.picker.is_none());
                    }));
            })
            .unwrap()
            .join()
            .unwrap();
    }
    #[test]
    fn actual_standard_app_event_loop_runs_on_one_mib_stack() {
        standard_app_one_mib_case(false);
        standard_app_one_mib_case(true);
    }

    fn standard_app_one_mib_case(pending_permission: bool) {
        std::thread::Builder::new()
            .name("standard-app-one-mib".into())
            .stack_size(1024 * 1024)
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(crate::startup::heap_future(|| async {
                        let root = crate::tests::OwnedTestDirectory::new("standard-app-one-mib");
                        eprintln!("standard App stack fixture: {}", root.path().display());
                        let _env = environment(&root);
                        let workspace =
                            Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
                        let cli = crate::cli::Cli {
                            no_session: true,
                            no_tools: true,
                            ..Default::default()
                        };
                        let cfg = config::Config {
                            session_title_model: Some("off".into()),
                            ..Default::default()
                        };
                        let mut session = Session::new("anthropic", "test", 128_000, "");
                        session.working_dir = root.path().to_string_lossy().into();
                        let mut context =
                            crate::context::load(true).for_workspace_binding(true, &workspace);
                        let mut app =
                            fixture_app(ui(&cli, &cfg, &mut session, &mut context, workspace));
                        let captured_workspace = app.ui.workspace.clone();
                        let captured_prompt = app.ui.context.current_prompt_name.clone();
                        let session_id = app.ui.session.id.clone();
                        let checker = crate::permission::checker::PermissionChecker::new(
                            &crate::permission::PermissionConfigs::default(),
                            crate::permission::SecurityMode::Standard,
                            Some(root.path().to_owned()),
                            None,
                        )
                        .unwrap();
                        let permission = Arc::new(std::sync::Mutex::new(checker));
                        app.ui.permission = Some(permission.clone());
                        let mut decision = if pending_permission {
                            let (reply, decision) = tokio::sync::oneshot::channel();
                            app.begin_permission_request(crate::permission::ask::AskRequest {
                                tool: "bash".into(),
                                input: "owned unanswered command".into(),
                                #[cfg(feature = "acp")]
                                tool_call_id: Some("owned-stack-call".into()),
                                suggested_pattern: None,
                                additional_allow_patterns: Vec::new(),
                                reply,
                            })
                            .unwrap();
                            Some(decision)
                        } else {
                            None
                        };
                        let events = [
                            UserEvent::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
                            UserEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
                            UserEvent::Key(KeyEvent::new(
                                KeyCode::Char('c'),
                                KeyModifiers::CONTROL,
                            )),
                            UserEvent::Paste("/quit".into()),
                            UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                        ];
                        let sender = app.user_tx.clone();
                        let feed = async move {
                            for event in events {
                                sender.send(event).await.unwrap();
                            }
                        };
                        tokio::time::timeout(Duration::from_secs(3), async {
                            let (_, result) =
                                tokio::join!(feed, crate::startup::heap_future(|| app.run_inner()));
                            result
                        })
                        .await
                        .unwrap()
                        .unwrap();
                        assert!(app.renderer.captured_output().contains("/quit"));
                        assert!(app.deferred_user_events.is_empty());
                        assert!(matches!(
                            app.user_rx.try_recv(),
                            Err(mpsc::error::TryRecvError::Empty)
                        ));
                        assert!(!app.run.is_running);
                        assert!(app.permission_prompt.is_none());
                        assert!(app.run.pending_turn.is_none());
                        assert!(app.ui.session.messages.is_empty());
                        assert_eq!(app.ui.session.id, session_id);
                        assert_eq!(app.ui.context.current_prompt_name, captured_prompt);
                        assert!(Arc::ptr_eq(&app.ui.workspace, &captured_workspace));
                        assert!(app.ui.session.permission_allowlist.is_empty());
                        assert_eq!(
                            permission.lock().unwrap().mode(),
                            crate::permission::SecurityMode::Guarded
                        );
                        if let Some(decision) = decision.as_mut() {
                            assert!(matches!(
                                decision.try_recv().unwrap(),
                                crate::permission::ask::UserDecision::Deny
                            ));
                        }
                    }));
            })
            .unwrap()
            .join()
            .unwrap();
    }
    #[tokio::test]
    async fn actual_app_title_retirement_joins_owned_work_charges_once_and_rejects_stale_result() {
        let root = crate::tests::OwnedTestDirectory::new("app-title-retirement");
        let _env = environment(&root);
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let cli = crate::cli::Cli {
            no_session: true,
            ..Default::default()
        };
        let cfg = config::Config::default();
        let mut session = Session::new("anthropic", "test", 128_000, "");
        session.working_dir = root.path().to_string_lossy().into();
        session.input_token_cost = 2.0;
        session.output_token_cost = 4.0;
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut app = fixture_app(ui(&cli, &cfg, &mut session, &mut context, workspace));
        let ledger = crate::session::title::UsageLedger::default();
        ledger.observe(rig::completion::Usage {
            input_tokens: 100,
            output_tokens: 5,
            total_tokens: 105,
            ..Default::default()
        });
        let (ready, started) = tokio::sync::oneshot::channel();
        let (retired, observed) = tokio::sync::oneshot::channel();
        struct Retired(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Retired {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let task = tokio::spawn(async move {
            let _retired = Retired(Some(retired));
            let _ = ready.send(());
            std::future::pending::<()>().await;
        });
        app.title_generation = 4;
        app.title_work = Some(SessionTitleWork {
            session_id: app.ui.session.id.clone(),
            generation: 4,
            workspace: app.ui.workspace.clone(),
            provider: "anthropic".into(),
            pricing: crate::session::title::TitlePricing {
                known: true,
                input: 2.0,
                output: 4.0,
                cache: Default::default(),
                anthropic_native: true,
            },
            ledger,
            work_scope: crate::agent::runner::AgentWorkScope::new(),
            task: Some(task),
        });
        tokio::time::timeout(Duration::from_secs(2), started)
            .await
            .unwrap()
            .unwrap();
        app.ui.session.input_token_cost = 200.0;
        app.retire_session_title().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), observed)
            .await
            .unwrap()
            .unwrap();
        assert!(app.title_work.is_none());
        assert_eq!(app.ui.session.total_input_tokens, 100);
        assert!((app.ui.session.total_cost - 0.00022).abs() < 1e-12);
        app.apply_session_title(SessionTitleResult {
            generation: 4,
            session_id: app.ui.session.id.clone(),
            title: Ok("STALE_TITLE".into()),
        })
        .await
        .unwrap();
        assert!(app.ui.session.generated_title.is_none());
        app.retire_session_title().await.unwrap();
        assert_eq!(app.ui.session.total_input_tokens, 100);
    }
}

#[cfg(test)]
mod usd_budget_app_tests {
    use super::test_support::fixture_app;
    use super::*;
    #[cfg(feature = "git-worktree")]
    #[tokio::test]
    async fn usd_budget_actual_app_git_proposal_is_denied_before_context_actor_or_request() {
        for case in ["zero", "ledger-only", "corrupt"] {
            let root = crate::tests::OwnedTestDirectory::new("cost-git-proposal-app");
            let bindings = [
                "ZS_CONFIG_DIR",
                "ZS_DATA_DIR",
                "ZS_LOCAL_DATA_DIR",
                "ZS_STATE_DIR",
                "ZS_CACHE_DIR",
                "ZS_CREDENTIALS_DIR",
            ]
            .map(|name| (name, Some(root.path().join(name).into_os_string())));
            let _env = crate::tests::ScopedProcessEnv::set(&bindings);
            let cli = crate::cli::Cli {
                no_sandbox: true,
                no_session: true,
                ..Default::default()
            };
            let mut cfg:crate::config::Config = serde_json::from_value(serde_json::json!({"max_budget_usd":0.0,"custom_providers":{"local-test":{"provider_type":"openai","base_url":"http://127.0.0.1:9/v1","api_style":"completions"}}})).unwrap();
            cfg.max_budget_usd = Some(if case == "zero" { 0.0 } else { 1.0 });
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new(
                "local-test",
                "fixture",
                128000,
                root.path().to_str().unwrap(),
            );
            session.set_input_output_prices(Some((1.0, 2.0)));
            session.initialize_spend_account(None).unwrap();
            let checkpoint = serde_json::to_vec(&session).unwrap();
            let ledger =
                crate::spend::Ledger::new(&crate::paths::process_paths().unwrap().state_dir);
            if case == "ledger-only" {
                session
                    .spend_account
                    .observe(&crate::spend::Receipt::new(
                        &session.id,
                        "local-test",
                        "fixture",
                        crate::spend::RequestKind::Turn,
                        crate::event::UsageDelta {
                            input_tokens: 2_000_000,
                            total_tokens: 2_000_000,
                            ..Default::default()
                        },
                        crate::spend::PriceSnapshot::capture(
                            Some((1.0, 2.0)),
                            Default::default(),
                            false,
                        ),
                    ))
                    .unwrap();
                ledger.flush(&session.spend_account).unwrap();
            } else if case == "corrupt" {
                let path = crate::paths::process_paths()
                    .unwrap()
                    .state_dir
                    .join("usage.jsonl");
                crate::fs::private_atomic_create_sync(&path, b"{\"crash\":").unwrap();
            }
            let bytes_before = std::fs::read(
                crate::paths::process_paths()
                    .unwrap()
                    .state_dir
                    .join("usage.jsonl"),
            )
            .ok();
            session = serde_json::from_slice(&checkpoint).unwrap();
            assert!(!session.spend_account.ledger_ready());
            assert_eq!(session.spend_account.snapshot().recorded_spend(), 0.0);

            let ui = UiContext {
                cli: &cli,
                cfg: &cfg,
                session: &mut session,
                context: &mut context,
                workspace,
                client: crate::provider::create_client(
                    "local-test",
                    Some("unused-key"),
                    &cfg.custom_providers_map(),
                    None,
                )
                .unwrap(),
                permission: None,
                ask_tx: None,
                sandbox: crate::sandbox::Sandbox::new(false, "none"),
                prebuild_invalidated: Arc::new(AtomicBool::new(false)),
                status_signals: None,
                #[cfg(feature = "skills")]
                skill_services: Arc::new(
                    crate::extras::js::skills::session::SkillServiceOwner::new(),
                ),
                #[cfg(feature = "mcp")]
                mcp_manager: None,
            };
            let mut app = fixture_app(ui);
            let error = app.begin_git_command("/pr").await.unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::spend::run::BudgetStopped>()
                    .is_some()
            );
            assert!(
                app.chain.git.work.is_none()
                    && app.run.main_abort.is_none()
                    && app.run.agent_rx.is_none()
            );
            assert!(!app.run.is_running && app.ui.session.messages.is_empty());
            assert_eq!(
                std::fs::read(
                    crate::paths::process_paths()
                        .unwrap()
                        .state_dir
                        .join("usage.jsonl")
                )
                .ok(),
                bytes_before
            );
            assert_eq!(
                app.ui.session.spend_account.snapshot().tracking_error,
                case == "corrupt"
            );
            if case == "ledger-only" {
                assert_eq!(app.ui.session.recorded_total_cost(), 2.0);
            }
        }
    }

    #[tokio::test]
    async fn usd_budget_actual_app_cost_query_is_readonly_and_queue_stays_visible() {
        let root = crate::tests::OwnedTestDirectory::new("cost-query-app");
        let bindings = [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ]
        .map(|name| (name, Some(root.path().join(name).into_os_string())));
        let _env = crate::tests::ScopedProcessEnv::set(&bindings);
        let cli = crate::cli::Cli {
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg:crate::config::Config=serde_json::from_value(serde_json::json!({"max_budget_usd":2.0,"custom_providers":{"local-test":{"provider_type":"openai","base_url":"http://127.0.0.1:9/v1","api_style":"completions"}}})).unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session =
            Session::new("local-test", "test", 128_000, root.path().to_str().unwrap());
        session.set_input_output_prices(Some((2.0, 4.0)));
        session.total_cost = 10.0;
        let paths = crate::paths::process_paths().unwrap();
        let ledger = crate::spend::Ledger::new(&paths.state_dir);
        session.configure_spend(Some(2.0), &ledger).unwrap();
        let receipt = crate::spend::Receipt::new(
            &session.id,
            "openai",
            "model\x1b]52;c;ZXZpbA==\x07",
            crate::spend::RequestKind::Turn,
            crate::event::UsageDelta {
                input_tokens: 1_000_000,
                ..Default::default()
            },
            crate::spend::PriceSnapshot::capture(Some((2.0, 4.0)), Default::default(), false),
        );
        session.spend_account.observe(&receipt).unwrap();
        ledger.flush(&session.spend_account).unwrap();
        let ledger_before = std::fs::read(paths.state_dir.join("usage.jsonl")).unwrap();
        let before = serde_json::to_vec(&session).unwrap();
        let ui = UiContext {
            cli: &cli,
            cfg: &cfg,
            session: &mut session,
            context: &mut context,
            workspace: workspace.clone(),
            client: crate::provider::create_client(
                "local-test",
                Some("unused-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox: crate::sandbox::Sandbox::new(false, "none"),
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        let mut app = fixture_app(ui);
        app.run.is_running = true;
        app.input.load_text(" \t/cost");
        crate::startup::heap_future(|| {
            app.handle_key_event(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ))
        })
        .await
        .unwrap();
        assert!(app.run.is_running);
        assert!(app.run.agent.is_none() && app.run.main_abort.is_none());
        assert!(app.run.pending_inputs.is_empty());
        app.run.is_running = false;
        let text = app.renderer.captured_output();
        for expected in [
            "Session recorded spend: $12.000000",
            "Legacy/imported estimate: $10.000000",
            "openai/model",
            "Today (UTC): $2.000000",
            "All time: $2.000000",
            "input 1000000",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(!text.contains("\x1b]52"));
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert_eq!(
            std::fs::read(paths.state_dir.join("usage.jsonl")).unwrap(),
            ledger_before
        );
        assert!(!paths.data_dir.join("sessions").exists());
        app.run.pending_inputs.push_back("queued follow-up".into());
        app.finalize_turn().await.unwrap();
        assert_eq!(
            app.run.pending_inputs.front().map(|s| s.as_str()),
            Some("queued follow-up")
        );
        assert!(!app.run.is_running && app.run.agent.is_none() && app.run.main_abort.is_none());
        assert!(
            app.renderer
                .captured_output()
                .contains("queued input is retained")
        );
        assert_eq!(
            app.ui.session.spend_account.snapshot().recorded_spend(),
            12.0
        );
    }
}

#[cfg(all(
    test,
    feature = "git-worktree",
    target_os = "macos",
    feature = "sandbox"
))]
mod git_command_consumer_tests {
    use super::test_support::fixture_app;
    use super::*;
    use crate::ui::slash::git::test_support::Repo;
    fn context<'a>(
        cli: &'a crate::cli::Cli,
        cfg: &'a config::Config,
        session: &'a mut Session,
        files: &'a mut crate::context::ContextFiles,
        repo: &Repo,
    ) -> UiContext<'a> {
        UiContext::new(
            cli,
            cfg,
            session,
            files,
            repo.workspace.clone(),
            crate::provider::create_client(
                "git32-fixture",
                Some("unused-local-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            None,
            None,
            repo.sandbox(),
            None,
        )
    }
    fn config() -> config::Config {
        serde_json::from_value(serde_json::json!({"session_title_model":"off","custom_providers":{"git32-fixture":{"provider_type":"openai","base_url":"http://127.0.0.1:9","api_style":"completions"}}})).unwrap()
    }
    fn history(repo: &Repo) -> Session {
        let mut session = Session::new("git32-fixture", "fixture", 128_000, "");
        session.working_dir = repo.workspace.root().to_string_lossy().into();
        session.add_message(crate::session::MessageRole::User, "edit only selected file");
        session.add_tool_call_with_id(
            "native-selected-write",
            "write",
            &serde_json::json!({"path":"selected.txt"}),
        );
        session.add_tool_result_with_id("native-selected-write", "write", "saved selected file");
        session
    }
    async fn key(app: &mut App<'_>, code: KeyCode, modifiers: KeyModifiers) {
        assert!(matches!(
            app.handle_user_event(UserEvent::Key(KeyEvent::new(code, modifiers)))
                .await
                .unwrap(),
            ControlFlow::Continue(())
        ));
    }
    async fn terminal(app: &mut App<'_>) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while app.run.is_running {
                let event = app
                    .run
                    .agent_rx
                    .as_mut()
                    .unwrap()
                    .recv()
                    .await
                    .expect("native actor should terminate");
                app.handle_agent_event(event).await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(app.run.agent_rx.is_none());
        assert!(app.chain.git.work.is_none());
    }
    #[tokio::test]
    async fn actual_app_commit_confirmation_decline_edit_and_modified_keys_cannot_execute() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let repo = Repo::seeded().await;
        let before = repo.git(&["rev-parse", "HEAD"]).await;
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let cfg = config();
        let mut session = history(&repo);
        let mut files = crate::context::load(true).for_workspace_binding(true, &repo.workspace);
        let mut app = fixture_app(context(&cli, &cfg, &mut session, &mut files, &repo));
        app.ui.permission = Some(repo.checker(crate::permission::PermissionConfig {
            default: Some(crate::permission::Action::Allow),
            ..Default::default()
        }));
        app.run_slash_command("/commit literal message")
            .await
            .unwrap();
        assert!(app.chain.git.pending.is_some());
        assert!(!app.run.is_running);
        assert!(app.renderer.captured_output().contains("selected.txt"));
        assert!(app.renderer.captured_output().contains("literal message"));
        key(&mut app, KeyCode::Char('y'), KeyModifiers::CONTROL).await;
        assert!(app.chain.git.pending.is_some());
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).await, before);
        key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE).await;
        assert!(app.chain.git.pending.is_none());
        assert!(app.renderer.captured_output().contains("declined"));
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).await, before);
        app.run_slash_command("/commit original").await.unwrap();
        key(&mut app, KeyCode::Char('b'), KeyModifiers::NONE).await;
        assert!(app.chain.git.editing);
        assert!(app.renderer.chain_prompt.is_none());
        assert!(matches!(
            app.handle_user_event(UserEvent::Paste(
                "edited literal $(NEVER_EXECUTE) 雪".into()
            ))
            .await
            .unwrap(),
            ControlFlow::Continue(())
        ));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE).await;
        assert!(!app.chain.git.editing);
        assert!(app.chain.git.pending.is_some());
        assert!(!app.run.is_running);
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).await, before);
        key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).await;
        terminal(&mut app).await;
        assert_eq!(
            repo.git(&["log", "-1", "--format=%B"]).await.trim(),
            "edited literal $(NEVER_EXECUTE) 雪"
        );
        assert_eq!(
            repo.git(&["show", "--format=", "--name-only", "HEAD"])
                .await
                .trim(),
            "selected.txt"
        );
        assert!(app.renderer.captured_output().contains("Committed"));
    }
    #[tokio::test]
    async fn actual_app_git_confirmation_rejects_successor_session_and_rebound_workspace() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let repo = Repo::seeded().await;
        let before = repo.git(&["rev-parse", "HEAD"]).await;
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let cfg = config();
        let mut session = history(&repo);
        let mut files = crate::context::load(true).for_workspace_binding(true, &repo.workspace);
        let mut app = fixture_app(context(&cli, &cfg, &mut session, &mut files, &repo));
        for replace_session in [true, false] {
            app.run_slash_command("/commit captured").await.unwrap();
            assert!(app.chain.git.pending.is_some());
            if replace_session {
                app.ui.session.id = uuid::Uuid::new_v4().to_string().into();
            } else {
                app.ui.workspace = Arc::new(
                    crate::paths::WorkspaceBinding::capture(repo.workspace.root()).unwrap(),
                );
            }
            key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).await;
            assert!(app.chain.git.pending.is_none());
            assert!(!app.run.is_running);
            assert!(app.chain.git.work.is_none());
            assert!(app.renderer.captured_output().contains("stale"));
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).await, before);
        }
    }
    #[tokio::test]
    async fn actual_app_diff_uses_authoritative_native_rows_without_ansi_injection() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let repo = Repo::seeded().await;
        std::fs::write(
            repo.directory.path().join("selected.txt"),
            "selected 雪\u{1b}[31mINERT\n",
        )
        .unwrap();
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let cfg = config();
        let mut session = history(&repo);
        let mut files = crate::context::load(true).for_workspace_binding(true, &repo.workspace);
        let mut app = fixture_app(context(&cli, &cfg, &mut session, &mut files, &repo));
        app.ui.permission = Some(repo.checker(crate::permission::PermissionConfig {
            default: Some(crate::permission::Action::Allow),
            ..Default::default()
        }));
        app.run_slash_command("/diff turn").await.unwrap();
        terminal(&mut app).await;
        let painted = app.renderer.captured_output();
        assert!(painted.contains("+selected 雪INERT"));
        assert!(painted.contains("-original"));
        assert!(!painted.contains("\u{1b}[31mINERT"));
        let calls:Vec<_>=app.ui.session.messages.iter().filter(|m|m.role==crate::session::MessageRole::ToolCall&&m.tool.as_ref().is_some_and(|tool|matches!(tool,crate::session::PersistedToolMessage::Call{name,..} if name=="git"))).collect();
        assert_eq!(calls.len(), 1);
        let id = calls[0].tool_call_id.as_deref().unwrap();
        assert!(
            app.ui
                .session
                .messages
                .iter()
                .any(|m| m.role == crate::session::MessageRole::ToolResult
                    && m.tool_call_id.as_deref() == Some(id))
        );
    }
    #[tokio::test]
    async fn actual_app_generated_commit_is_tool_free_charged_once_and_requires_confirmation() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let repo = Repo::seeded().await;
        let before = repo.git(&["rev-parse", "HEAD"]).await;
        let mut wire = crate::provider::git_proposal_test_support::Wire::new(
            r#"{"message":"generated native proposal"}"#,
        );
        let cfg = wire.config();
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let mut session = history(&repo);
        session.provider = "git32-wire".into();
        session.input_token_cost = 2.0;
        session.output_token_cost = 4.0;
        session.calibrated_tokens = 777;
        let mut files = crate::context::load(true).for_workspace_binding(true, &repo.workspace);
        let ui = UiContext::new(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            repo.workspace.clone(),
            crate::provider::create_client(
                "git32-wire",
                Some("unused-owned-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            None,
            None,
            repo.sandbox(),
            None,
        );
        let mut app = fixture_app(ui);
        app.ui.permission = Some(repo.checker(crate::permission::PermissionConfig {
            default: Some(crate::permission::Action::Allow),
            ..Default::default()
        }));
        app.run_slash_command("/commit").await.unwrap();
        assert!(app.run.is_running);
        terminal(&mut app).await;
        let body = wire.body();
        assert_eq!(body["max_tokens"], 1024);
        assert!(
            body.get("tools")
                .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
        );
        assert!(body.to_string().contains("selected.txt"));
        assert!(body.to_string().contains("selected changed"));
        assert!(app.chain.git.pending.is_some());
        assert!(
            app.renderer
                .captured_output()
                .contains("generated native proposal")
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).await, before);
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .await
                .is_empty()
        );
        assert_eq!(app.ui.session.total_input_tokens, 7);
        assert_eq!(app.ui.session.total_output_tokens, 5);
        assert!((app.ui.session.total_cost - 0.000034).abs() < 1e-12);
        assert_eq!(app.ui.session.calibrated_tokens, 777);
        key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE).await;
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).await, before);
        assert!((app.ui.session.total_cost - 0.000034).abs() < 1e-12);
    }
    #[tokio::test]
    async fn actual_app_malformed_generated_proposal_keeps_observed_usage_without_effects() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let repo = Repo::seeded().await;
        let before = repo.git(&["rev-parse", "HEAD"]).await;
        let mut wire = crate::provider::git_proposal_test_support::Wire::new(
            r#"{"message":"never","paths":["unrelated.txt"]}"#,
        );
        let cfg = wire.config();
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let mut session = history(&repo);
        session.provider = "git32-wire".into();
        session.input_token_cost = 2.0;
        session.output_token_cost = 4.0;
        let mut files = crate::context::load(true).for_workspace_binding(true, &repo.workspace);
        let ui = UiContext::new(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            repo.workspace.clone(),
            crate::provider::create_client(
                "git32-wire",
                Some("unused-owned-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            None,
            None,
            repo.sandbox(),
            None,
        );
        let mut app = fixture_app(ui);
        app.ui.permission = Some(repo.checker(crate::permission::PermissionConfig {
            default: Some(crate::permission::Action::Allow),
            ..Default::default()
        }));
        app.run_slash_command("/commit").await.unwrap();
        terminal(&mut app).await;
        wire.body();
        assert!(app.chain.git.pending.is_none());
        assert!(app.renderer.captured_output().contains("proposal rejected"));
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).await, before);
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .await
                .is_empty()
        );
        assert_eq!(app.ui.session.total_input_tokens, 7);
        assert!((app.ui.session.total_cost - 0.000034).abs() < 1e-12);
    }
    #[tokio::test]
    async fn actual_app_native_permission_cancel_settles_call_drops_queue_and_cannot_late_grant() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let repo = Repo::seeded().await;
        let before = repo.git(&["rev-parse", "HEAD"]).await;
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let cfg = config();
        let mut session = history(&repo);
        let mut files = crate::context::load(true).for_workspace_binding(true, &repo.workspace);
        let mut app = fixture_app(context(&cli, &cfg, &mut session, &mut files, &repo));
        app.ui.permission = Some(repo.checker(crate::permission::PermissionConfig {
            default: Some(crate::permission::Action::Allow),
            git_diff: Some(crate::permission::ToolPerm::Simple(
                crate::permission::Action::Ask,
            )),
            ..Default::default()
        }));
        let (asks, requests) = tokio::sync::mpsc::channel(4);
        app.ui.ask_tx = Some(asks.clone());
        app.ask_rx = Some(requests);
        app.run_slash_command("/diff turn").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.permission_prompt.is_none() {
                match tokio::time::timeout(Duration::from_millis(50), app.run_inner()).await {
                    Err(_) => {}
                    Ok(result) => panic!("App exited during controlled wait: {result:?}"),
                }
            }
        })
        .await
        .expect("native Ask must become visible");
        assert!(app.renderer.captured_output().contains("git/diff"));
        assert!(app.run.is_running);
        app.run
            .pending_inputs
            .extend(["STALE_QUEUED_ONE".into(), "STALE_QUEUED_TWO".into()]);
        app.input.load_text("recoverable native follow-up");
        app.user_tx
            .send(UserEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .await
            .unwrap();
        match tokio::time::timeout(Duration::from_millis(200), app.run_inner()).await {
            Err(_) => {}
            Ok(result) => panic!("App exited during controlled cancel: {result:?}"),
        }
        assert!(app.run.agent_rx.is_none() && app.run.main_abort.is_none());
        assert!(app.chain.git.work.is_none() && app.chain.git.pending.is_none());
        assert!(
            app.permission_prompt.is_none()
                && !app.run.is_running
                && app.run.pending_inputs.is_empty()
        );
        assert_eq!(app.input.buffer, "recoverable native follow-up");
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert!(
            app.ui
                .session
                .messages
                .iter()
                .all(|m| !m.content.contains("STALE_QUEUED"))
        );
        let calls: Vec<_> = app
            .ui
            .session
            .messages
            .iter()
            .filter(|m| m.role == crate::session::MessageRole::ToolCall)
            .collect();
        let id = calls.last().unwrap().tool_call_id.as_deref().unwrap();
        assert!(
            app.ui
                .session
                .messages
                .iter()
                .any(|m| m.role == crate::session::MessageRole::ToolResult
                    && m.tool_call_id.as_deref() == Some(id))
        );
        assert!(
            app.renderer
                .captured_output()
                .contains("dropped 2 queued inputs")
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).await, before);
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .await
                .is_empty()
        );
        key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).await;
        assert!(!app.run.is_running && app.ui.session.permission_allowlist.is_empty());
    }
    #[tokio::test]
    async fn actual_app_generated_pr_requires_preview_and_shell_ask_before_gh() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let repo = Repo::seeded().await;
        repo.gh_stub(
            "printf '%s\\0' \"$@\" > gh-app-argv; printf '%s\\n' https://example.invalid/pull/32",
        );
        let mut wire = crate::provider::git_proposal_test_support::Wire::new(
            r#"{"title":"PR literal 雪","body":"body $(NEVER_EXECUTE)"}"#,
        );
        let cfg = wire.config();
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let mut session = history(&repo);
        session.provider = "git32-wire".into();
        let mut files = crate::context::load(true).for_workspace_binding(true, &repo.workspace);
        let ui = UiContext::new(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            repo.workspace.clone(),
            crate::provider::create_client(
                "git32-wire",
                Some("unused-owned-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            None,
            None,
            repo.sandbox(),
            None,
        );
        let mut app = fixture_app(ui);
        app.ui.permission = Some(repo.checker(crate::permission::PermissionConfig {
            default: Some(crate::permission::Action::Allow),
            bash: Some(crate::permission::ToolPerm::Simple(
                crate::permission::Action::Ask,
            )),
            ..Default::default()
        }));
        let (asks, requests) = tokio::sync::mpsc::channel(4);
        app.ui.ask_tx = Some(asks);
        app.ask_rx = Some(requests);
        app.run_slash_command("/pr").await.unwrap();
        terminal(&mut app).await;
        let body = wire.body();
        assert_eq!(body["max_tokens"], 1024);
        assert!(
            body.get("tools")
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
        );
        assert!(app.chain.git.pending.is_some());
        assert!(app.renderer.captured_output().contains("PR literal 雪"));
        assert!(!repo.workspace.root().join("gh-app-argv").exists());
        key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.permission_prompt.is_none() {
                match tokio::time::timeout(Duration::from_millis(50), app.run_inner()).await {
                    Err(_) => {}
                    Ok(result) => panic!("App exited before shell Ask: {result:?}"),
                }
            }
        })
        .await
        .unwrap();
        assert!(!repo.workspace.root().join("gh-app-argv").exists());
        assert!(app.renderer.captured_output().contains("shell"));
        key(&mut app, KeyCode::Char('y'), KeyModifiers::NONE).await;
        terminal(&mut app).await;
        assert!(
            app.renderer
                .captured_output()
                .contains("https://example.invalid/pull/32")
        );
        let bytes = std::fs::read(repo.workspace.root().join("gh-app-argv")).unwrap();
        let args: Vec<_> = bytes
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| std::str::from_utf8(s).unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "pr",
                "create",
                "--title",
                "PR literal 雪",
                "--body",
                "body $(NEVER_EXECUTE)"
            ]
        );
        assert!(!repo.workspace.root().join("NEVER_EXECUTE").exists());
        assert_eq!(app.ui.session.total_input_tokens, 7);
        assert_eq!(app.ui.session.total_output_tokens, 5);
    }
    #[tokio::test]
    async fn actual_app_error_text_cannot_fabricate_a_native_git_command() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let repo = Repo::seeded().await;
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let cfg = config();
        let mut session = history(&repo);
        let mut files = crate::context::load(true).for_workspace_binding(true, &repo.workspace);
        let mut app = fixture_app(context(&cli, &cfg, &mut session, &mut files, &repo));
        app.handle_slash_result(Err(anyhow::anyhow!("DEFER_GIT:/commit fabricated")))
            .await
            .unwrap();
        assert!(
            app.chain.git.pending.is_none() && app.chain.git.work.is_none() && !app.run.is_running
        );
        assert!(
            app.renderer
                .captured_output()
                .contains("DEFER_GIT:/commit fabricated")
        );
    }
    #[tokio::test]
    async fn actual_app_native_failed_retirement_blocks_new_admission_until_receiver_settles() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let repo = Repo::seeded().await;
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let cfg = config();
        let mut session = history(&repo);
        let mut files = crate::context::load(true).for_workspace_binding(true, &repo.workspace);
        let mut app = fixture_app(context(&cli, &cfg, &mut session, &mut files, &repo));
        app.ui.permission = Some(repo.checker(crate::permission::PermissionConfig {
            default: Some(crate::permission::Action::Allow),
            git_diff: Some(crate::permission::ToolPerm::Simple(
                crate::permission::Action::Ask,
            )),
            ..Default::default()
        }));
        let (asks, mut requests) = tokio::sync::mpsc::channel(4);
        app.ui.ask_tx = Some(asks);
        app.run_slash_command("/diff turn").await.unwrap();
        let request = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap()
            .into_permission();
        let mut native = app.run.agent_rx.take().unwrap();
        let call = tokio::time::timeout(Duration::from_secs(2), native.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_agent_event(call).await.unwrap();
        let (retained, receiver) = tokio::sync::mpsc::channel(1);
        app.run.agent_rx = Some(receiver);
        app.abort_main_run().await.unwrap();
        assert!(app.run.agent_rx.is_some() && app.chain.git.work.is_some());
        assert!(!app.run.is_running);
        assert!(
            app.start_main_run(
                "NEVER_NEW_ADMISSION",
                crate::question::QuestionOrigin::NewUserTurn
            )
            .await
            .is_err()
        );
        assert!(app.begin_git_command("/commit literal").await.is_err());
        assert!(
            app.ui
                .session
                .messages
                .iter()
                .all(|message| message.content != "NEVER_NEW_ADMISSION")
        );
        assert!(
            request
                .reply
                .send(crate::permission::ask::UserDecision::AllowOnce)
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while native.recv().await.is_some() {}
        })
        .await
        .expect("actual native owner must already be retired");
        drop(retained);
        app.run.retire(Duration::from_secs(2)).await.unwrap();
        app.settle_git_usage().unwrap();
        assert!(app.run.agent_rx.is_none() && app.chain.git.work.is_none());
        let id = app
            .ui
            .session
            .messages
            .iter()
            .rev()
            .find(|message| message.role == crate::session::MessageRole::ToolCall)
            .unwrap()
            .tool_call_id
            .as_deref()
            .unwrap();
        assert!(app.ui.session.messages.iter().any(|message| message.role
            == crate::session::MessageRole::ToolResult
            && message.tool_call_id.as_deref() == Some(id)));
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .await
                .is_empty()
        );
    }
}

#[cfg(all(test, not(feature = "git-worktree")))]
mod git_feature_off_consumer_tests {
    use super::test_support::fixture_app;
    use super::*;
    #[tokio::test]
    async fn actual_app_feature_off_git_commands_refuse_without_runner_or_proposal() {
        let root = crate::tests::OwnedTestDirectory::new("git32-feature-off");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "ZS_CREDENTIALS_DIR",
                Some(root.path().join("credentials").into_os_string()),
            ),
        ]);
        let cfg:config::Config=serde_json::from_value(serde_json::json!({"session_title_model":"off","retry":{"max_attempts":1},"custom_providers":{"git32-feature-off":{"provider_type":"openrouter","base_url":"http://127.0.0.1:1"}}})).unwrap();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_context_files: true,
            no_sandbox: true,
            ..Default::default()
        };
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut session = Session::new(
            "git32-feature-off",
            "fixture",
            128_000,
            root.path().to_str().unwrap(),
        );
        let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
        let ui = UiContext::new(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            workspace,
            crate::provider::create_client(
                "git32-feature-off",
                Some("unused-owned-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            None,
            None,
            crate::sandbox::Sandbox::new(false, "none"),
            None,
        );
        let mut app = fixture_app(ui);
        for command in ["/diff", "/commit literal", "/pr"] {
            app.run_slash_command(command).await.unwrap();
            assert!(
                !app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none()
            );
        }
        assert_eq!(
            (0..app.renderer.feed().block_count())
                .filter_map(|index| app.renderer.feed().block_text(index))
                .filter(|text| text.contains("Git commands require the git-worktree feature"))
                .count(),
            3
        );
        assert!(app.ui.session.messages.is_empty());
    }
}

#[cfg(test)]
mod context_diagnostic_tests {
    use super::test_support::fixture_app;
    use super::*;

    fn saved_files(directory: &std::path::Path) -> Vec<std::ffi::OsString> {
        let mut entries: Vec<_> = match std::fs::read_dir(directory) {
            Ok(entries) => entries.map(|entry| entry.unwrap().file_name()).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("cannot inspect owned persistence root: {error}"),
        };
        entries.sort();
        entries
    }

    #[tokio::test]
    async fn actual_context_and_doctor_are_pure_cold_busy_and_built_without_provider_requests() {
        let root = crate::tests::OwnedTestDirectory::new("context-diagnostic-app");
        let environment = [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ]
        .map(|name| (name, Some(root.path().join(name).into_os_string())));
        let _environment = crate::tests::ScopedProcessEnv::set(&environment);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cli = crate::cli::Cli {
            tools: vec!["read".into()],
            no_sandbox: true,
            api_key: Some("owned-unused-key".into()),
            system_prompt: Some("OVERRIDE_PERSONA".into()),
            append_system_prompt: Some("CAPTURED_APPEND_界".into()),
            ..Default::default()
        };
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({"custom_providers":{"context-local-test":{"provider_type":"openai","base_url":format!("http://{address}/v1"),"api_style":"completions"}}})).unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        context.agents = Some("CAPTURED_AGENTS_界".into());
        context.current_prompt = Some("CAPTURED_PROMPT_界".into());
        let added = root.path().join("added.md");
        std::fs::write(&added, "CAPTURED_EXTRA_界").unwrap();
        context.extra_files.push(added.clone());
        context
            .extra_file_contents
            .insert(added.clone(), Arc::new("CAPTURED_EXTRA_界".into()));
        let paths = crate::paths::process_paths().unwrap();
        let suffix = paths.suffix_file();
        std::fs::create_dir_all(suffix.parent().unwrap()).unwrap();
        std::fs::write(&suffix, "CAPTURED_SUFFIX_界").unwrap();
        let mut session = Session::new(
            "context-local-test",
            "test-model",
            128_000,
            root.path().to_str().unwrap(),
        );
        session.add_message(crate::session::MessageRole::User, "PRIVATE_USER_CONTENT");
        #[cfg(feature = "goal")]
        {
            let mut goal =
                crate::extras::goal::Goal::new("PENDING_FIRST_ROUND", Vec::new()).unwrap();
            goal.context_file = Some(root.path().join("UNREAD_PENDING_PLAN.md"));
            session.goal_store.set(goal, false).unwrap();
        }

        let ui = UiContext {
            cli: &cli,
            cfg: &cfg,
            session: &mut session,
            context: &mut context,
            workspace,
            client: crate::provider::create_client(
                "context-local-test",
                Some("owned-unused-key"),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap(),
            permission: None,
            ask_tx: None,
            sandbox: crate::sandbox::Sandbox::new(false, "none"),
            prebuild_invalidated: Arc::new(AtomicBool::new(false)),
            status_signals: None,
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        let mut app = fixture_app(ui);
        let (overhead, parts) = crate::agent::builder::estimate_overhead_breakdown(
            app.ui.context,
            false,
            &cli,
            &cfg,
            &app.ui.sandbox,
            None,
        );
        app.ui.session.overhead_tokens = overhead;
        app.ui.session.overhead_parts = parts;
        let stored_before = saved_files(&paths.sessions_dir());
        let before = serde_json::to_vec(app.ui.session).unwrap();
        let overhead_before = (
            app.ui.session.overhead_tokens,
            app.ui.session.overhead_parts.clone(),
        );
        app.run.is_running = true;
        assert_eq!(
            super::super::classify_submission(true, "/context", &app.ui.context.prompts),
            super::super::SubmitAction::Run
        );
        app.input.load_text(" \t/context");
        crate::startup::heap_future(|| {
            app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        })
        .await
        .unwrap();
        assert!(app.run.agent.is_none() && app.run.main_abort.is_none());
        assert!(app.run.pending_inputs.is_empty());
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert_eq!(
            (
                app.ui.session.overhead_tokens,
                app.ui.session.overhead_parts.clone()
            ),
            overhead_before
        );
        assert_eq!(
            saved_files(&paths.sessions_dir()),
            stored_before,
            "diagnostic must not save session"
        );
        let output = app.renderer.captured_output();
        assert!(output.contains("Context composition:") && output.contains("agent not built"));
        assert!(!output.contains("PRIVATE_USER_CONTENT"));
        app.input.load_text(" \t/doctor");
        crate::startup::heap_future(|| {
            app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        })
        .await
        .unwrap();
        let output = app.renderer.captured_output();
        assert!(
            output.contains("price source: captured session rates")
                && output.contains("Terminal attachment:")
        );
        assert!(app.run.agent.is_none() && app.run.main_abort.is_none());
        assert!(app.run.pending_inputs.is_empty() && app.run.is_running);
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert_eq!(saved_files(&paths.sessions_dir()), stored_before);
        app.run.is_running = false;
        std::fs::create_dir_all(root.path().join(".zerostack")).unwrap();
        std::fs::write(
            root.path().join(".zerostack/config.toml"),
            "model='test-model'\n",
        )
        .unwrap();
        crate::config::load::trust_project_config(
            &root.path().join(".zerostack/config.toml"),
            &paths.project_config_trust_file(),
        )
        .unwrap();
        std::fs::write(
            root.path().join("CLAUDE.local.md"),
            "PRIVATE_LOCAL_INSTRUCTION_BODY",
        )
        .unwrap();
        let built = {
            let build_context = app.ui.agent_build_ctx();
            crate::startup::heap_future(|| build_context.rebuild_agent("test-model", false)).await
        };
        let definitions = crate::startup::heap_future(|| built.context_tool_definitions())
            .await
            .unwrap();
        assert!(
            definitions
                .iter()
                .any(|definition| definition.name == "read")
        );
        let definition_count = definitions.len();
        let snapshot = built.context_snapshot().unwrap();
        let preamble = built.context_preamble_for_test();
        assert!(preamble.contains("PRIVATE_LOCAL_INSTRUCTION_BODY"));
        assert_eq!(
            snapshot
                .instruction_sources
                .as_ref()
                .unwrap()
                .diagnostics()
                .unwrap()
                .source_count,
            1
        );
        assert_eq!(snapshot.bytes, preamble.len());
        assert_eq!(snapshot.tokens, Session::estimate_tokens(preamble));
        let mut cursor = 0;
        let mut reconstructed = String::new();
        for ((label, range), (token_label, tokens)) in snapshot.ranges.iter().zip(&snapshot.parts) {
            assert_eq!(range.start, cursor);
            assert_eq!(label, token_label);
            assert!(preamble.is_char_boundary(range.start) && preamble.is_char_boundary(range.end));
            assert_eq!(*tokens, Session::estimate_tokens(&preamble[range.clone()]));
            reconstructed.push_str(&preamble[range.clone()]);
            cursor = range.end;
        }
        assert_eq!(cursor, preamble.len());
        assert_eq!(reconstructed.as_bytes(), preamble.as_bytes());
        let markers = [
            "OVERRIDE_PERSONA",
            "CAPTURED_AGENTS_界",
            "CAPTURED_PROMPT_界",
            "CAPTURED_EXTRA_界",
            "CAPTURED_APPEND_界",
            "CAPTURED_SUFFIX_界",
        ];
        let positions: Vec<_> = markers
            .iter()
            .map(|marker| preamble.find(marker).unwrap())
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            snapshot
                .parts
                .iter()
                .any(|(label, _)| *label == "Repository instructions")
        );
        let saved_snapshot = snapshot.parts.clone();
        let saved_tokens = snapshot.tokens;
        app.run.agent = Some(built);
        // Changes after build cannot alter the diagnostic or cause a reread.
        std::fs::remove_file(&suffix).unwrap();
        std::fs::remove_file(&added).unwrap();
        std::fs::remove_file(root.path().join("CLAUDE.local.md")).unwrap();
        #[cfg(unix)]
        {
            drop(crate::tests::process_gate::ProcessGate::new(&suffix).unwrap());
            drop(crate::tests::process_gate::ProcessGate::new(&added).unwrap());
            drop(
                crate::tests::process_gate::ProcessGate::new(&root.path().join("CLAUDE.local.md"))
                    .unwrap(),
            );
        }
        app.renderer.clear_content().unwrap();
        crate::startup::heap_future(|| app.run_slash_command("/context"))
            .await
            .unwrap();
        let output = app.renderer.captured_output();
        assert!(output.contains("actual built-agent preamble snapshot"));
        assert!(output.contains("CLAUDE.local.md"));
        assert!(!output.contains("PRIVATE_LOCAL_INSTRUCTION_BODY"));
        assert!(output.contains(&format!(
            "{definition_count} already-registered definitions"
        )));
        assert!(
            app.run.main_abort.is_none() && !app.run.is_running,
            "read-only diagnostics must not start a pending goal round"
        );

        assert!(!output.contains("CAPTURED_") && !output.contains("PRIVATE_USER_CONTENT"));
        assert_eq!(
            app.run
                .agent
                .as_ref()
                .unwrap()
                .context_snapshot()
                .unwrap()
                .parts,
            saved_snapshot
        );
        assert_eq!(
            app.run
                .agent
                .as_ref()
                .unwrap()
                .context_snapshot()
                .unwrap()
                .tokens,
            saved_tokens
        );
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert!(saved_files(&paths.sessions_dir()) == stored_before);
        crate::startup::heap_future(|| app.run_slash_command("/doctor"))
            .await
            .unwrap();
        // Traverse the actual report scrollback; rich feature reports are
        // intentionally longer than one viewport.
        for _ in 0..4 {
            crate::startup::heap_future(|| {
                app.handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            })
            .await
            .unwrap();
            app.renderer.render_viewport().unwrap();
        }
        let output = app.renderer.captured_output();
        assert!(output.contains("Directories:") && output.contains("Provider and model:"));
        assert!(!output.contains("CAPTURED_") && !output.contains("PRIVATE_USER_CONTENT"));
        assert!(app.run.main_abort.is_none() && !app.run.is_running);
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert_eq!(saved_files(&paths.sessions_dir()), stored_before);
        app.renderer.scroll_to_bottom().unwrap();
        crate::startup::heap_future(|| app.run_slash_command("/doctor unexpected"))
            .await
            .unwrap();
        assert!(app.renderer.captured_output().contains("usage: /doctor"));
        crate::startup::heap_future(|| app.run_slash_command("/context unexpected"))
            .await
            .unwrap();
        assert!(app.renderer.captured_output().contains("usage: /context"));
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert!(
            tokio::time::timeout(Duration::from_millis(80), listener.accept())
                .await
                .is_err(),
            "diagnostic must not warm provider cache or make a model request"
        );
    }
}

#[cfg(all(test, feature = "git-worktree"))]
mod git_confirmation_stack_tests {
    use super::test_support::fixture_app;
    use super::*;
    #[test]
    fn actual_combined_context_and_git_confirmation_event_loop_runs_on_one_mib_stack() {
        std::thread::Builder::new()
            .name("combined-context-git-one-mib".into())
            .stack_size(1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(crate::startup::heap_future(|| async {
                        #[cfg(feature = "hooks")]
                        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
                        let root = crate::tests::OwnedTestDirectory::new("combined-git-one-mib");
                        let bindings = [
                            "ZS_CONFIG_DIR",
                            "ZS_DATA_DIR",
                            "ZS_LOCAL_DATA_DIR",
                            "ZS_STATE_DIR",
                            "ZS_CACHE_DIR",
                            "ZS_CREDENTIALS_DIR",
                        ]
                        .map(|name| (name, Some(root.path().join(name).into_os_string())));
                        let _environment = crate::tests::ScopedProcessEnv::set(&bindings);
                        let workspace =
                            Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
                        std::fs::write(root.path().join("selected.txt"), "selected changed雪\n")
                            .unwrap();
                        std::fs::write(root.path().join("unrelated.txt"), "unrelated unchanged\n")
                            .unwrap();
                        let cli = crate::cli::Cli {
                            no_session: true,
                            no_tools: true,
                            ..Default::default()
                        };
                        let cfg: config::Config = serde_json::from_value(serde_json::json!({
                            "session_title_model":"off", "custom_providers":{"git32-fixture":{
                                "provider_type":"openai", "base_url":"http://127.0.0.1:9/v1", "api_style":"completions"
                            }}
                        })).unwrap();
                        let mut session = Session::new(
                            "git32-fixture",
                            "unused-stack-fixture",
                            128_000,
                            root.path().to_str().unwrap(),
                        );
                        session.add_message(
                            crate::session::MessageRole::User,
                            "edit only selected file",
                        );
                        session.add_tool_call_with_id(
                            "native-selected-write",
                            "write",
                            &serde_json::json!({"path":"selected.txt"}),
                        );
                        session.add_tool_result_with_id(
                            "native-selected-write",
                            "write",
                            "saved selected file",
                        );
                        let before = serde_json::to_vec(&session).unwrap();
                        let mut files =
                            crate::context::load(true).for_workspace_binding(true, &workspace);
                        let client = crate::provider::create_client(
                            "git32-fixture",
                            Some("unused-local-key"),
                            &cfg.custom_providers_map(),
                            cfg.api_keys.as_ref(),
                        )
                        .unwrap();
                        let ui = UiContext::new(
                            &cli,
                            &cfg,
                            &mut session,
                            &mut files,
                            workspace.clone(),
                            client,
                            None,
                            None,
                            crate::sandbox::Sandbox::new(false, "none")
                                .with_workspace_binding(workspace),
                            None,
                        );
                        let mut app = fixture_app(ui);
                        let captured = app.ui.workspace.clone();
                        // App initialization captures runtime overhead, so snapshot afterwards.
                        let initialized = serde_json::to_vec(app.ui.session).unwrap();
                        let sender = app.user_tx.clone();
                        let producer = async move {
                            for event in [
                                UserEvent::Paste("  /context  ".into()),
                                UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                                UserEvent::Paste("/doctor".into()),
                                UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                                UserEvent::Paste("/commit ONE_MIB_DECLINED".into()),
                                UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                                UserEvent::Key(KeyEvent::new(
                                    KeyCode::Char('n'),
                                    KeyModifiers::NONE,
                                )),
                                UserEvent::Paste("/context".into()),
                                UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                                UserEvent::Paste("/quit".into()),
                                UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                            ] {
                                sender.send(event).await.unwrap();
                            }
                        };
                        tokio::time::timeout(Duration::from_secs(5), async {
                            let (_, result) = tokio::join!(
                                producer,
                                crate::startup::heap_future(|| app.run_inner())
                            );
                            result
                        })
                        .await
                        .unwrap()
                        .unwrap();
                        let rendered = app.renderer.captured_output();
                        assert!(rendered.contains("Context composition:"));
                        assert!(rendered.contains("Terminal attachment:"));
                        assert!(rendered.contains("ONE_MIB_DECLINED"));
                        assert!(rendered.contains("Git proposal declined"));
                        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), initialized);
                        assert_eq!(
                            app.ui.session.messages.len(),
                            serde_json::from_slice::<Session>(&before)
                                .unwrap()
                                .messages
                                .len()
                        );
                        assert!(Arc::ptr_eq(&app.ui.workspace, &captured));
                        assert!(
                            app.run.agent.is_none()
                                && app.run.agent_rx.is_none()
                                && app.run.main_abort.is_none()
                        );
                        assert!(app.chain.git.pending.is_none() && app.chain.git.work.is_none());
                        assert_eq!(
                            std::fs::read_to_string(root.path().join("selected.txt")).unwrap(),
                            "selected changed雪\n"
                        );
                        assert_eq!(
                            std::fs::read_to_string(root.path().join("unrelated.txt")).unwrap(),
                            "unrelated unchanged\n"
                        );
                        assert!(
                            !root.path().join(".git").exists(),
                            "fixture never executes Git"
                        );
                    }));
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

#[cfg(feature = "agent-skills")]
fn skill_description(text: &str) -> String {
    let mut text = sanitize_output(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.len() > 384 {
        let mut end = 381;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

#[cfg(test)]
mod plan_approval_tests {
    use super::test_support::fixture_app;
    use super::*;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode};
    use crate::session::MessageRole;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const PLAN: &str = "Plan雪: update only the selected flag.\nVerify its test. $(literal)";

    struct OwnedApp<'a>(App<'a>);
    impl<'a> std::ops::Deref for OwnedApp<'a> {
        type Target = App<'a>;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl std::ops::DerefMut for OwnedApp<'_> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
    impl Drop for OwnedApp<'_> {
        fn drop(&mut self) {
            self.0.cancel_pending_permissions();
            self.0.run.cancel_validation();
            if let Some(abort) = self.0.run.main_abort.take() {
                abort.abort();
            }
            self.0.ui.sandbox.kill_active();
        }
    }
    fn owned_app(ui: UiContext<'_>) -> OwnedApp<'_> {
        OwnedApp(fixture_app(ui))
    }

    struct Provider {
        address: std::net::SocketAddr,
        requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        task: tokio::task::JoinSet<()>,
    }
    impl Provider {
        async fn new(responses: Vec<&'static str>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured = requests.clone();
            let mut task = tokio::task::JoinSet::new();
            task.spawn(async move {
                for text in responses {
                    tokio::time::timeout(Duration::from_secs(15), async {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let mut bytes = Vec::new();
                        let (header, length) = loop {
                            let mut chunk = [0; 4096];
                            let count = stream.read(&mut chunk).await.unwrap();
                            assert_ne!(count, 0, "owned provider header closed");
                            bytes.extend_from_slice(&chunk[..count]);
                            assert!(bytes.len() <= 256 * 1024);
                            if let Some(header) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                                let header_text = String::from_utf8_lossy(&bytes[..header]);
                                let length = header_text.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap();
                                break (header + 4, length);
                            }
                        };
                        assert!(length <= 256 * 1024);
                        while bytes.len() < header + length {
                            let mut chunk = [0; 4096];
                            let count = stream.read(&mut chunk).await.unwrap();
                            assert_ne!(count, 0, "owned provider body closed");
                            bytes.extend_from_slice(&chunk[..count]);
                        }
                        captured.lock().unwrap().push(serde_json::from_slice(&bytes[header..header+length]).unwrap());
                        if text == "hold" {
                            std::future::pending::<()>().await;
                        }
                        if text == "hold-open" {
                            let chunk = serde_json::json!({"id":"owned-plan80","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"STREAM_WITHOUT_FINAL"},"finish_reason":null}]});
                            let body = format!("data: {chunk}\n\n");
                            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n", body.len()).as_bytes()).await.unwrap();
                            std::future::pending::<()>().await;
                        }
                        let (status, body, kind) = if text == "error" {
                            ("400 Bad Request", "{\"error\":{\"message\":\"owned planning failure\"}}".into(), "application/json")
                        } else {
                            let delta = if matches!(text, "question-batch" | "question-again") {
                                serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("{text}-id"),"type":"function","function":{"name":"ask_user_question","arguments":serde_json::json!({"questions":[
                                    {"question":"Which branch?","options":[{"label":"main","description":"Primary branch"}],"allow_free_text":false},
                                    {"question":"What should change?","allow_free_text":true}
                                ]}).to_string()}}]})
                            } else if text == "skills-search" {
                                serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"instruction-search-one","type":"function","function":{"name":"skills_search","arguments":"{\"query\":\"code review\"}"}}]})
                            } else if text == "todo-progress67" {
                                serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"todo67","type":"function","function":{"name":"todo_write","arguments":serde_json::json!({"todos":[
                                    {"content":"done","status":"completed","priority":"medium"},
                                    {"content":"cancelled","status":"cancelled","priority":"medium"},
                                    {"content":"pending","status":"pending","priority":"medium"}
                                ]}).to_string()}}]})
                            } else if text == "unknown-tool" {
                                serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"unknown-cost-initial","type":"function","function":{"name":"unknown_unregistered_cost_tool","arguments":"{}"}}]})
                            } else if text == "cap-write" {
                                serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"write-cost-initial","type":"function","function":{"name":"write","arguments":"{\"path\":\"effect.txt\",\"content\":\"ACTUAL_OWNED_EFFECT\"}"}}]})
                            } else if text == "pressure-read" {
                                serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"read-cost-compact","type":"function","function":{"name":"read","arguments":"{\"path\":\"held.txt\"}"}}]})
                            } else if text == "ask-read" {
                                serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"read-plan80","type":"function","function":{"name":"read","arguments":"{\"path\":\"held.txt\"}"}}]})
                            } else { serde_json::json!({"role":"assistant","content":text}) };
                            let first = serde_json::json!({"id":"owned-plan80","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                            let last = serde_json::json!({"id":"owned-plan80","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":if matches!(text,"ask-read"|"cap-write"|"unknown-tool"|"pressure-read"|"question-batch"|"question-again"|"skills-search"|"todo-progress67") {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":if text=="pressure-read" {100000} else {9},"completion_tokens":4,"total_tokens":if text=="pressure-read" {100004} else {13}}});
                            ("200 OK",format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),"text/event-stream")
                        };
                        let wire = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                        stream.write_all(wire.as_bytes()).await.unwrap();
                        stream.shutdown().await.unwrap();
                    }).await.expect("owned local provider must be bounded");
                }
            });
            Self {
                address,
                requests,
                task,
            }
        }
        fn config(&self) -> config::Config {
            serde_json::from_value(serde_json::json!({
                "session_title_model":"off", "compact":false,
                "custom_providers":{"plan80":{"provider_type":"openai","base_url":format!("http://{}/v1",self.address),"api_style":"completions"}}
            })).unwrap()
        }
        fn calls(&self) -> Vec<serde_json::Value> {
            self.requests.lock().unwrap().clone()
        }
        async fn finish(mut self) {
            while let Some(result) = self.task.join_next().await {
                result.unwrap();
            }
        }
        async fn cancel(mut self) {
            self.task.shutdown().await;
        }
    }

    struct Roots {
        directory: crate::tests::OwnedTestDirectory,
        _environment: crate::tests::ScopedProcessEnv,
    }
    impl Roots {
        fn new() -> Self {
            let directory = crate::tests::OwnedTestDirectory::new("actual-plan-approval");
            let bindings = [
                "ZS_CONFIG_DIR",
                "ZS_DATA_DIR",
                "ZS_LOCAL_DATA_DIR",
                "ZS_STATE_DIR",
                "ZS_CACHE_DIR",
                "ZS_CREDENTIALS_DIR",
            ]
            .map(|name| (name, Some(directory.path().join(name).into_os_string())));
            let environment = crate::tests::ScopedProcessEnv::set(&bindings);
            Self {
                directory,
                _environment: environment,
            }
        }
        fn workspace(&self) -> Arc<crate::paths::WorkspaceBinding> {
            Arc::new(crate::paths::WorkspaceBinding::capture(self.directory.path()).unwrap())
        }
    }
    fn ui<'a>(
        cli: &'a crate::cli::Cli,
        cfg: &'a config::Config,
        session: &'a mut Session,
        context: &'a mut crate::context::ContextFiles,
        workspace: Arc<crate::paths::WorkspaceBinding>,
        mode: SecurityMode,
    ) -> UiContext<'a> {
        let policy = PermissionConfigs::from(PermissionConfig {
            default: Some(Action::Deny),
            ..Default::default()
        });
        let checker = crate::permission::checker::PermissionChecker::new(
            &policy,
            mode,
            Some(workspace.root().to_owned()),
            None,
        )
        .unwrap();
        let client = crate::provider::create_client(
            "plan80",
            Some("unused-local-key"),
            &cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap();
        UiContext::new(
            cli,
            cfg,
            session,
            context,
            workspace.clone(),
            client,
            Some(Arc::new(std::sync::Mutex::new(checker))),
            None,
            crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace),
            None,
        )
    }
    async fn user(app: &mut App<'_>, event: UserEvent) {
        app.user_tx.send(event).await.unwrap();
        let delivered = app.user_rx.recv().await.unwrap();
        assert!(matches!(
            crate::startup::heap_future(|| app.handle_user_event(delivered))
                .await
                .unwrap(),
            ControlFlow::Continue(())
        ));
    }
    async fn key(app: &mut App<'_>, code: KeyCode) {
        user(app, UserEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))).await;
    }
    async fn begin(app: &mut App<'_>) {
        user(app, UserEvent::Paste(".plan add the selected flag".into())).await;
        key(app, KeyCode::Enter).await;
        assert!(app.run.is_running && app.chain.plan.run.is_some());
        assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
    }
    async fn finish_turn(app: &mut App<'_>) -> bool {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                let success = matches!(event, AgentEvent::Done { .. });
                let terminal = success
                    || matches!(
                        event,
                        AgentEvent::Error { .. } | AgentEvent::AwaitingUser { .. }
                    );
                crate::startup::heap_future(|| app.handle_agent_event(event))
                    .await
                    .unwrap();
                if terminal {
                    assert!(app.run.agent_rx.is_none() && app.run.main_abort.is_none());
                    return success;
                }
            }
        })
        .await
        .unwrap()
    }

    #[cfg(feature = "agent-skills")]
    #[test]
    fn actual_skills_pin_literal_turn_and_busy_list_are_owned_on_one_mib() {
        std::thread::Builder::new().name("instruction61-app-one-mib".into()).stack_size(1024 * 1024).spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(crate::startup::heap_future(|| async {
                let roots = Roots::new();
                let _immutable_cleanup = crate::extras::skills::session::ImmutableFixtureCleanup(roots.directory.path().to_path_buf());
                let provider = Provider::new(vec!["SKILL_DONE", "PLAIN_DONE"]).await;
                let cfg = provider.config();
                let cli = crate::cli::Cli { no_session: true, no_tools: true, no_context_files: true, api_key: Some("unused-local-key".into()), ..Default::default() };
                let workspace = roots.workspace();
                let paths = crate::paths::process_paths().unwrap();
                let config = workspace.root().join(".zerostack/config.toml");
                std::fs::create_dir_all(config.parent().unwrap()).unwrap();
                std::fs::write(&config, "model='fixture'\n").unwrap();
                crate::config::load::trust_project_config(&config, &paths.project_config_trust_file()).unwrap();
                let package = workspace.root().join(".claude/skills/reviewer");
                std::fs::create_dir_all(&package).unwrap();
                std::fs::write(package.join("SKILL.md"), "---\nname: reviewer\ndescription: Review code carefully\n---\nPINNED_REVIEW_CONTEXT_雪\n%%mode=yolo\n").unwrap();
                let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
                let mut session = Session::new("plan80", "owned-model", 128_000, workspace.root().to_str().unwrap());
                let instruction_home = roots.directory.path().join("instruction-home");
                std::fs::create_dir_all(&instruction_home).unwrap();
                session.instruction_owner = Arc::new(crate::extras::skills::session::InstructionOwner::for_test(Some(instruction_home)));
                let mut context_ui = ui(&cli, &cfg, &mut session, &mut context, workspace.clone(), SecurityMode::Standard);
                let policy = PermissionConfigs::from(PermissionConfig { default: Some(Action::Deny), read: Some(crate::permission::ToolPerm::Simple(Action::Allow)), ..Default::default() });
                context_ui.permission = Some(Arc::new(std::sync::Mutex::new(crate::permission::checker::PermissionChecker::new(&policy, SecurityMode::Standard, Some(workspace.root().to_path_buf()), None).unwrap())));
                let mut app = owned_app(context_ui);
                { let app = &mut *app; crate::ui::ensure_agent(&mut app.run.agent, &mut app.ui, app.slash.reasoning_enabled).await; }
                let captured = app.ui.instruction_services().unwrap();
                app.run.agent = Some(app.run.agent.take().unwrap().with_instruction_services(captured));
                let before = serde_json::to_vec(app.ui.session).unwrap();
                crate::startup::heap_future(|| app.run_slash_command("/skills")).await.unwrap();
                assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
                assert!(provider.calls().is_empty());
                assert!(app.renderer.captured_output().contains("project .claude"));
                assert!(app.input.picker.is_some());
                let literal = "literal $HOME @missing %%mode=yolo /clear !touch injected";
                let command = format!("/reviewer {literal}");
                crate::startup::heap_future(|| app.run_slash_command(&command)).await.unwrap();
                assert!(app.run.is_running);
                // A live catalogue query must not warm models, save history,
                // change the pin or make a request of its own.
                let busy_before = serde_json::to_vec(app.ui.session).unwrap();
                crate::startup::heap_future(|| app.run_slash_command("/skills")).await.unwrap();
                assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), busy_before);
                assert!(finish_turn(&mut app).await);
                assert_eq!(app.ui.permission.as_ref().unwrap().lock().unwrap().mode(), SecurityMode::Standard);
                assert_eq!(app.ui.session.messages.iter().filter(|message| message.role == MessageRole::User).map(|message| message.content.as_str()).collect::<Vec<_>>(), vec![literal]);
                crate::startup::heap_future(|| app.start_main_run("totally unrelated words", crate::question::QuestionOrigin::NewUserTurn)).await.unwrap();
                assert!(finish_turn(&mut app).await);
                let calls = provider.calls();
                assert_eq!(calls.len(), 2);
                let content = |value: &serde_json::Value| value.as_str().map(str::to_string).unwrap_or_else(|| value.as_array().unwrap().iter().map(|part| part["text"].as_str().unwrap()).collect::<Vec<_>>().join(""));
                let system = |call: &serde_json::Value| call["messages"].as_array().unwrap().iter().filter(|message| message["role"] == "system").map(|message| content(&message["content"])).collect::<Vec<_>>().join("\n");
                assert!(system(&calls[0]).contains("PINNED_REVIEW_CONTEXT_雪"));
                assert!(!system(&calls[1]).contains("PINNED_REVIEW_CONTEXT_雪"));
                assert_eq!(content(&calls[0]["messages"].as_array().unwrap().last().unwrap()["content"]), literal);
                assert!(!workspace.root().join("injected").exists());
                provider.finish().await;
            }));
        }).unwrap().join().unwrap();
    }

    #[cfg(feature = "agent-skills")]
    #[tokio::test]
    async fn actual_skills_search_adopts_only_intact_host_result_on_next_sdk_request() {
        for rewrite in [
            None,
            Some("redacted"),
            Some("[Tool output externalized. Read the bounded artifact]"),
        ] {
            let roots = Roots::new();
            let _cleanup = crate::extras::skills::session::ImmutableFixtureCleanup(
                roots.directory.path().to_path_buf(),
            );
            let provider = Provider::new(vec!["skills-search", "SEARCH_DONE"]).await;
            let cfg = provider.config();
            let cli = crate::cli::Cli {
                no_session: true,
                no_context_files: true,
                tools: vec!["skills_search".into()],
                api_key: Some("unused-local-key".into()),
                ..Default::default()
            };
            let workspace = roots.workspace();
            let paths = crate::paths::process_paths().unwrap();
            let package = roots.directory.path().join("reviewer");
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(
                package.join("SKILL.md"),
                "---\nname: reviewer\ndescription: code review\n---\nSEARCH_SELECTED_INSTRUCTION\n",
            )
            .unwrap();
            crate::extras::skills::import_agent_skill(&package, &paths).unwrap();
            let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new(
                "plan80",
                "owned-model",
                128_000,
                workspace.root().to_str().unwrap(),
            );
            session.instruction_owner = Arc::new(
                crate::extras::skills::session::InstructionOwner::for_test(None),
            );
            let context_ui = ui(
                &cli,
                &cfg,
                &mut session,
                &mut files,
                workspace.clone(),
                SecurityMode::Standard,
            );
            let mut app = owned_app(context_ui);
            {
                let app = &mut *app;
                crate::ui::ensure_agent(
                    &mut app.run.agent,
                    &mut app.ui,
                    app.slash.reasoning_enabled,
                )
                .await;
            }
            let services = app.ui.instruction_services().unwrap();
            let mut agent = app
                .run
                .agent
                .take()
                .unwrap()
                .with_instruction_services(services);
            if let Some(text) = rewrite {
                agent = agent.with_instruction_test_result_rewrite(text);
            }
            app.run.agent = Some(agent);
            crate::startup::heap_future(|| {
                app.start_main_run(
                    "unrelated initial topic",
                    crate::question::QuestionOrigin::NewUserTurn,
                )
            })
            .await
            .unwrap();
            assert!(finish_turn(&mut app).await);
            let calls = provider.calls();
            assert_eq!(calls.len(), 2);
            let first = serde_json::to_string(&calls[0]["messages"][0]).unwrap();
            let second = serde_json::to_string(&calls[1]["messages"][0]).unwrap();
            assert!(!first.contains("SEARCH_SELECTED_INSTRUCTION"));
            assert_eq!(
                second.contains("SEARCH_SELECTED_INSTRUCTION"),
                rewrite.is_none(),
                "{second}"
            );
            let result = serde_json::to_string(&calls[1]["messages"]).unwrap();
            if let Some(text) = rewrite {
                assert!(result.contains(text));
                assert!(!result.contains("agent_skills"));
            } else {
                assert!(result.contains("reviewer"));
            }
            assert_eq!(
                app.ui
                    .session
                    .messages
                    .iter()
                    .filter(|m| m.role == MessageRole::User)
                    .count(),
                1
            );
            provider.finish().await;
        }
    }

    #[cfg(feature = "agent-skills")]
    #[tokio::test]
    async fn actual_skill_pin_raw_override_fails_before_history_or_http() {
        let roots = Roots::new();
        let _cleanup = crate::extras::skills::session::ImmutableFixtureCleanup(
            roots.directory.path().to_path_buf(),
        );
        let provider = Provider::new(vec![]).await;
        let mut cfg = provider.config();
        cfg.extra_body =
            Some(serde_json::json!({"messages":[{"role":"user","content":"raw override"}]}));
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_context_files: true,
            api_key: Some("unused-local-key".into()),
            ..Default::default()
        };
        let workspace = roots.workspace();
        let paths = crate::paths::process_paths().unwrap();
        let package = roots.directory.path().join("reviewer");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("SKILL.md"),
            "---\nname: reviewer\ndescription: code review\n---\nPIN_RAW_GUARDED\n",
        )
        .unwrap();
        crate::extras::skills::import_agent_skill(&package, &paths).unwrap();
        let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.instruction_owner = Arc::new(
            crate::extras::skills::session::InstructionOwner::for_test(None),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            workspace,
            SecurityMode::Standard,
        ));
        let before = serde_json::to_vec(app.ui.session).unwrap();
        crate::startup::heap_future(|| app.run_slash_command("/reviewer literal args"))
            .await
            .unwrap();
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
        assert!(app.renderer.captured_output().contains("incompatible"));
        assert!(provider.calls().is_empty());
        provider.finish().await;
    }

    #[tokio::test]
    async fn busy67_actual_picker_native_write_error_is_propagated() {
        let roots = Roots::new();
        let workspace = roots.workspace();
        let provider = Provider::new(Vec::new()).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_context_files: true,
            no_tools: true,
            api_key: Some("unused-local-key".into()),
            ..Default::default()
        };
        let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("plan80", "owned-model", 128_000, "picker-native-error");
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            workspace,
            SecurityMode::Standard,
        ));
        let native_fault = crate::ui::pickers::test_surface::NativeFaultGuard::new(true);
        let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
            app.user_tx
                .send(UserEvent::Key(KeyEvent::new(
                    KeyCode::Char('/'),
                    KeyModifiers::NONE,
                )))
                .await
                .unwrap();
            let event = app.user_rx.recv().await.unwrap();
            let error = crate::startup::heap_future(|| app.handle_user_event(event))
                .await
                .unwrap_err();
            let cause = error
                .downcast_ref::<std::io::Error>()
                .expect("picker IO failure retains its exact cause");
            assert_eq!(cause.kind(), std::io::ErrorKind::WouldBlock);
            assert_eq!(
                cause.to_string(),
                "controlled native picker writer would block"
            );
            assert_eq!(
                native_fault.counts(),
                (1, 1),
                "geometry succeeded before actual native writer failure"
            );
            assert!(matches!(
                app.input.picker,
                Some(crate::ui::input::Picker::Command(_))
            ));
            assert!(app.ui.session.messages.is_empty() && app.ui.context.extra_files.is_empty());
            assert!(provider.calls().is_empty());
        }))
        .await;
        provider.cancel().await;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[tokio::test]
    async fn busy67_actual_add_completion_tracks_captured_workspace_without_chdir() {
        let roots = Roots::new();
        let second = crate::tests::OwnedTestDirectory::new("add67-second-root");
        let second_workspace =
            Arc::new(crate::paths::WorkspaceBinding::capture(second.path()).unwrap());
        let process_cwd = std::env::current_dir().unwrap();
        let first_workspace = roots.workspace();
        std::fs::write(
            first_workspace.root().join("only_first67.txt"),
            "FIRST_ONLY_CONTENT",
        )
        .unwrap();
        let chosen = "only_second67 雪.txt";
        std::fs::write(second_workspace.root().join(chosen), "SECOND_ONLY_CONTENT").unwrap();
        let provider = Provider::new(Vec::new()).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_context_files: true,
            no_tools: true,
            api_key: Some("unused-local-key".into()),
            ..Default::default()
        };
        let mut files = crate::context::load(true).for_workspace_binding(true, &first_workspace);
        let mut session = Session::new("plan80", "owned-model", 128_000, "completion67");
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            first_workspace,
            SecurityMode::Standard,
        ));
        let native_fault = crate::ui::pickers::test_surface::NativeFaultGuard::new(false);
        let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
            for code in [
                KeyCode::Char('/'),
                KeyCode::Char('a'),
                KeyCode::Char('d'),
                KeyCode::Char('d'),
                KeyCode::Enter,
            ] {
                key(&mut app, code).await;
                #[cfg(feature = "mcp")]
                if code == KeyCode::Char('/') {
                    app.input
                        .set_mcp_resources(vec!["mcp:foreign:urn%3AONLY_RESOURCE67".into()]);
                }
            }
            busy67_finish_file_walk(&mut app).await;
            let Some(crate::ui::input::Picker::File(picker)) = &app.input.picker else {
                panic!("add command picker missing")
            };
            assert!(
                picker
                    .matches
                    .iter()
                    .any(|path| path == std::path::Path::new("only_first67.txt"))
            );
            assert!(
                !picker
                    .matches
                    .iter()
                    .any(|path| path == std::path::Path::new("Cargo.toml")
                        || path.to_string_lossy().contains("mcp:"))
            );
            key(&mut app, KeyCode::Esc).await;
            app.input.clear_buffer();
            // This is the same captured UiContext field changed by workspace/session
            // activation; the process cwd is deliberately left unchanged.
            app.ui.workspace = second_workspace.clone();
            app.ui.sandbox = app
                .ui
                .sandbox
                .clone()
                .with_workspace_binding(second_workspace.clone());
            user(&mut app, UserEvent::Paste("/add only_second67".into())).await;
            key(&mut app, KeyCode::Tab).await;
            busy67_finish_file_walk(&mut app).await;
            let Some(crate::ui::input::Picker::File(picker)) = &app.input.picker else {
                panic!("captured add picker missing")
            };
            assert_eq!(picker.matches, vec![std::path::PathBuf::from(chosen)]);
            key(&mut app, KeyCode::Enter).await;
            let selected = app.input.buffer.to_string();
            assert_eq!(
                crate::ui::slash::split_args(&selected).unwrap(),
                vec!["/add", chosen]
            );
            assert!(app.ui.context.extra_files.is_empty() && app.ui.session.messages.is_empty());
            crate::startup::heap_future(|| app.run_slash_command(&selected))
                .await
                .unwrap();
            let expected = second_workspace.root().join(chosen);
            assert_eq!(app.ui.context.extra_files, vec![expected.clone()]);
            assert_eq!(
                app.ui.context.extra_file_contents[&expected].as_str(),
                "SECOND_ONLY_CONTENT"
            );
            assert_eq!(std::env::current_dir().unwrap(), process_cwd);
            assert!(provider.calls().is_empty());
            assert_eq!(
                native_fault.counts(),
                (0, 0),
                "captured picker cannot access native geometry or writer"
            );
            let rendered = app.renderer.captured_output();
            assert!(rendered.contains("/add") && rendered.contains(chosen));
        }))
        .await;
        provider.cancel().await;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    async fn busy67_finish_file_walk(app: &mut App<'_>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let Some(crate::ui::input::Picker::File(picker)) = &mut app.input.picker else {
                    panic!("file picker missing")
                };
                picker.try_finish_loading();
                if !picker.is_loading() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn busy67_actual_jobs_list_and_stop_keep_the_running_job_owned() {
        let roots = Roots::new();
        let provider = Provider::new(vec!["hold-open"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_context_files: true,
            no_tools: true,
            api_key: Some("unused-local-key".into()),
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("plan80", "owned-model", 128_000, "job control");
        let ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            workspace,
            SecurityMode::Standard,
        );
        let mut app = owned_app(ui);
        let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
            user(&mut app, UserEvent::Paste("held main turn".into())).await;
            key(&mut app, KeyCode::Enter).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while provider.calls().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let id = app
                .ui
                .sandbox
                .start_background_command(
                    "printf BG_%s READY67; sleep 60".into(),
                    Duration::from_secs(20),
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !app
                    .ui
                    .sandbox
                    .background_job_snapshot(&id)
                    .unwrap()
                    .stdout
                    .windows(10)
                    .any(|bytes| bytes == b"BG_READY67")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let before = serde_json::to_vec(app.ui.session).unwrap();
            crate::startup::heap_future(|| app.run_slash_command("/jobs"))
                .await
                .unwrap();
            assert!(app.renderer.captured_output().contains(&id));
            assert!(
                !app.renderer.captured_output().contains("BG_READY67"),
                "job output is not listing metadata"
            );
            let stop_command = format!("/jobs stop {id}");
            crate::startup::heap_future(|| app.run_slash_command(&stop_command))
                .await
                .unwrap();
            assert!(
                app.renderer
                    .captured_output()
                    .contains("stop requested; the job remains owned")
            );
            assert_eq!(
                app.ui.sandbox.background_job_summaries().len(),
                1,
                "stop cannot release the registry record"
            );
            let terminal = app
                .ui
                .sandbox
                .wait_background_job(&id, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(
                terminal.status,
                crate::sandbox::BackgroundJobStatus::Cancelled
            );
            assert!(!terminal.descendants_escaped);
            assert_eq!(app.ui.sandbox.running_background_job_count(), 0);
            assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
            assert!(app.run.is_running && app.run.agent_rx.is_some());
            assert_eq!(provider.calls().len(), 1);
        }))
        .await;
        crate::startup::heap_future(|| app.abort_main_run())
            .await
            .unwrap();
        provider.cancel().await;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn busy67_actual_views_preserve_held_run_and_reject_mutating_selectors() {
        std::thread::Builder::new()
            .name("busy67-readonly-one-mib".into())
            .stack_size(1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(crate::startup::heap_future(busy67_actual_views_case));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    async fn busy67_actual_views_case() {
        let roots = Roots::new();
        let provider = Provider::new(vec!["todo-progress67", "hold-open"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_context_files: true,
            tools: vec!["todo_write".into()],
            api_key: Some("unused-local-key".into()),
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut saved = Session::new(
            "plan80",
            "saved-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        saved.name = "SAVED_METADATA67".into();
        saved.working_dir = workspace.root().to_string_lossy().into();
        saved.add_message(crate::session::MessageRole::User, "saved first message");
        crate::session::storage::save_session(&saved).unwrap();
        let saved_path = crate::session::storage::session_path(&saved.id).unwrap();
        let saved_bytes = std::fs::read(&saved_path).unwrap();
        let mut ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            workspace.clone(),
            SecurityMode::Standard,
        );
        let policy = PermissionConfigs::from(PermissionConfig {
            default: Some(Action::Allow),
            ..Default::default()
        });
        ui.permission = Some(Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &policy,
                SecurityMode::Standard,
                Some(workspace.root().to_owned()),
                None,
            )
            .unwrap(),
        )));
        let mut app = owned_app(ui);
        app.slash.todo_tools_enabled = true;
        let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
            user(&mut app, UserEvent::Paste("held ordinary turn".into())).await;
            key(&mut app, KeyCode::Enter).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                    let ready =
                        matches!(&event, AgentEvent::ToolResult { name, .. } if name=="todo_write");
                    crate::startup::heap_future(|| app.handle_agent_event(event))
                        .await
                        .unwrap();
                    if ready {
                        break;
                    }
                }
                while provider.calls().len() != 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(app.ui.session.todos.progress(), Some((2, 3)));
            let (_, footer) = app.renderer.last_statusline();
            assert!(
                footer.iter().flatten().any(|span| matches!(
                    span, crate::ui::statusline::StatusSpan::Text { text, .. }
                    if text == "todo:2/3"
                )),
                "the actual tool result refreshes the sticky footer before another input"
            );
            app.run.pending_inputs.push_back("queued next turn".into());
            let before = serde_json::to_vec(app.ui.session).unwrap();
            let model = app.ui.session.model.clone();
            let id = app.ui.session.id.clone();
            let account = app.ui.session.spend_account.snapshot();
            for command in ["/help", "/model", "/sessions", "/sessions --all", "/jobs"] {
                crate::startup::heap_future(|| app.run_slash_command(command))
                    .await
                    .unwrap();
            }
            key(&mut app, KeyCode::Char('?')).await;
            assert!(app.input.buffer.is_empty());
            assert!(app.renderer.captured_output().contains("SAVED_METADATA67"));
            assert!(app.renderer.captured_output().contains("keys:"));
            assert!(
                app.renderer
                    .captured_output()
                    .contains("current model: owned-model")
            );
            assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
            assert_eq!(std::fs::read(&saved_path).unwrap(), saved_bytes);
            assert!(Arc::ptr_eq(&app.ui.workspace, &workspace));
            assert_eq!(app.ui.session.spend_account.snapshot(), account);
            assert_eq!(app.run.pending_inputs.len(), 1);
            assert!(
                app.run.is_running && app.run.agent_rx.is_some() && app.run.main_abort.is_some()
            );
            assert_eq!(
                provider.calls().len(),
                2,
                "views must not warm another model or start queued work"
            );
            for command in [
                "/model changed",
                &format!("/sessions {}", saved.id),
                &format!("/sessions delete {}", saved.id),
            ] {
                crate::startup::heap_future(|| app.run_slash_command(command))
                    .await
                    .unwrap();
            }
            let catalogue = crate::session::catalogue::list_readonly(&workspace, false).unwrap();
            app.input
                .start_session_picker(catalogue, workspace.clone(), false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&saved_path, std::fs::Permissions::from_mode(0o644))
                    .unwrap();
            }
            assert!(
                app.handle_session_picker_event(&UserEvent::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE
                )))
                .await
                .unwrap()
            );
            assert_eq!(app.ui.session.id, id);
            assert_eq!(app.ui.session.model, model);
            assert_eq!(std::fs::read(&saved_path).unwrap(), saved_bytes);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&saved_path).unwrap().permissions().mode() & 0o777,
                    0o644,
                    "stale selector must reject before loading or repairing its record"
                );
            }
            assert_eq!(provider.calls().len(), 2);
        }))
        .await;
        crate::startup::heap_future(|| app.abort_main_run())
            .await
            .unwrap();
        provider.cancel().await;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[tokio::test]
    async fn actual_plan_turn_asks_before_second_request_and_restores_exact_operator_ceiling() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        for mode in [SecurityMode::Standard, SecurityMode::ReadOnly] {
            let roots = Roots::new();
            let provider = Provider::new(vec![PLAN, "IMPLEMENTED"]).await;
            let cfg = provider.config();
            let cli = crate::cli::Cli {
                no_session: true,
                no_tools: true,
                ..Default::default()
            };
            let workspace = roots.workspace();
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new(
                "plan80",
                "owned-model",
                128_000,
                workspace.root().to_str().unwrap(),
            );
            let mut app = owned_app(ui(&cli, &cfg, &mut session, &mut context, workspace, mode));
            begin(&mut app).await;
            app.input.load_text("typed follow-up雪");
            app.run
                .pending_inputs
                .extend(["stale queued one".into(), "stale queued two".into()]);
            assert!(finish_turn(&mut app).await);
            assert_eq!(provider.calls().len(), 1);
            assert!(
                app.renderer
                    .captured_output()
                    .contains(crate::ui::plan::QUESTION)
            );
            assert_eq!(app.chain.plan.pending.as_ref().unwrap().plan, PLAN);
            assert_eq!(app.input.buffer, "typed follow-up雪");
            assert_eq!(app.run.pending_inputs.len(), 2);
            let granted = serde_json::to_value(&app.ui.session.permission_allowlist).unwrap();
            let approved = if mode == SecurityMode::ReadOnly {
                key(&mut app, KeyCode::Char('e')).await;
                app.input
                    .load_text("Edited approved plan雪\n.plan !literal");
                key(&mut app, KeyCode::Enter).await;
                assert_eq!(provider.calls().len(), 1);
                "Edited approved plan雪\n.plan !literal"
            } else {
                PLAN
            };
            key(&mut app, KeyCode::Char('y')).await;
            assert!(app.run.is_running);
            assert!(app.run.pending_inputs.is_empty());
            assert_eq!(
                app.ui.permission.as_ref().unwrap().lock().unwrap().mode(),
                mode
            );
            assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("code"));
            assert!(finish_turn(&mut app).await);
            assert_eq!(
                serde_json::to_value(&app.ui.session.permission_allowlist).unwrap(),
                granted
            );
            assert!(
                crate::agent::tools::check_perm(&app.ui.permission, &None, "write", "selected.txt")
                    .await
                    .is_err()
            );
            let calls = provider.calls();
            assert_eq!(calls.len(), 2);
            assert!(calls[0].to_string().contains("Planning-Only Mode"));
            assert!(calls[1].to_string().contains("Coding Mode"));
            let submitted = app
                .ui
                .session
                .messages
                .iter()
                .rfind(|message| message.role == MessageRole::User)
                .unwrap()
                .content
                .to_string();
            assert!(submitted.starts_with("Execute this approved plan"));
            assert!(submitted.ends_with(approved));
            assert_eq!(
                calls[1]["messages"].as_array().unwrap().last().unwrap()["content"].as_str(),
                Some(submitted.as_str())
            );
            assert!(
                calls[1]
                    .to_string()
                    .contains("Approved plan (literal text)")
            );
            assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
            app.input.clear_buffer();
            key(&mut app, KeyCode::Char('y')).await;
            assert_eq!(
                provider.calls().len(),
                2,
                "duplicate Y cannot admit a new turn"
            );
            provider.finish().await;
        }
    }

    #[tokio::test]
    async fn usd_budget_actual_approved_plan_retains_literal_draft_without_second_request() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec![PLAN]).await;
        let mut cfg = provider.config();
        // The owned provider reports 9 input and 4 output tokens. This is a
        // real exhausted paid cap, rather than an unknown-price refusal.
        cfg.max_budget_usd = Some(34.0 / 1_000_000.0);
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.set_input_output_prices(Some((2.0, 4.0)));
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        begin(&mut app).await;
        assert!(finish_turn(&mut app).await);
        assert_eq!(provider.calls().len(), 1);
        assert_eq!(app.chain.plan.pending.as_ref().unwrap().plan, PLAN);
        let before = serde_json::to_vec(app.ui.session).unwrap();
        assert!(
            (app.ui.session.spend_account.snapshot().recorded_spend()
                - cfg.max_budget_usd.unwrap())
            .abs()
                < 1e-12
        );
        key(&mut app, KeyCode::Char('y')).await;
        assert_eq!(provider.calls().len(), 1);
        assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
        assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
        assert!(app.run.pending_inputs.is_empty());
        assert!(
            app.input.buffer.starts_with("Execute this approved plan")
                && app.input.buffer.ends_with(PLAN)
        );
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert!(app.ui.session.permission_allowlist.is_empty());
        drop(app);
        provider.finish().await;
    }

    #[tokio::test]
    async fn usd_budget_initial_interactive_terminal_persists_receipt_before_another_turn() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec![PLAN, "IMPLEMENTED"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.set_input_output_prices(Some((2.0, 4.0)));
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        begin(&mut app).await;
        assert!(finish_turn(&mut app).await);
        let ledger = crate::spend::Ledger::new(&roots.directory.path().join("ZS_STATE_DIR"));
        let first = ledger.summary_for_session(&app.ui.session.id).unwrap();
        assert_eq!(
            first.session.records, 1,
            "terminal settlement must persist this turn without a future request"
        );
        assert_eq!(first.session.input_tokens, 9);
        assert_eq!(first.session.output_tokens, 4);
        assert!((first.session.known_cost - 0.000034).abs() < 1e-12);
        assert!(!app.ui.session.spend_account.has_pending());
        assert!(!app.ui.session.spend_account.snapshot().tracking_error);
        key(&mut app, KeyCode::Char('y')).await;
        assert!(finish_turn(&mut app).await);
        let second = ledger.summary_for_session(&app.ui.session.id).unwrap();
        assert_eq!(second.session.records, 2);
        assert_eq!(second.session.input_tokens, 18);
        assert_eq!(second.session.output_tokens, 8);
        assert!((second.session.known_cost - 0.000068).abs() < 1e-12);
        assert!(
            (app.ui.session.spend_account.snapshot().recorded_spend() - second.session.known_cost)
                .abs()
                < 1e-12
        );
        assert_eq!(provider.calls().len(), 2);
        assert!(app.ui.session.permission_allowlist.is_empty());
        crate::session::storage::save_session(app.ui.session).unwrap();
        let saved = crate::session::storage::load_session_exact(&app.ui.session.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            (saved.total_input_tokens, saved.total_output_tokens),
            (18, 8)
        );
        assert!((saved.total_cost - 0.000068).abs() < 1e-12);
        assert!(!saved.spend_account.snapshot().tracking_error);
        drop(app);
        provider.finish().await;
    }

    #[tokio::test]
    async fn usd_budget_initial_interactive_write_has_same_policy_positive_and_cap_negative() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        for capped in [false, true] {
            let roots = Roots::new();
            let provider = Provider::new(if capped {
                vec!["cap-write"]
            } else {
                vec!["cap-write", "WRITE_COMPLETED"]
            })
            .await;
            let mut cfg = provider.config();
            cfg.max_budget_usd = capped.then_some(0.000034);
            let cli = crate::cli::Cli {
                no_session: true,
                ..Default::default()
            };
            let workspace = roots.workspace();
            let effect = workspace.root().join("effect.txt");
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new(
                "plan80",
                "owned-model",
                128_000,
                workspace.root().to_str().unwrap(),
            );
            session.set_input_output_prices(Some((2.0, 4.0)));
            let mut ui = ui(
                &cli,
                &cfg,
                &mut session,
                &mut context,
                workspace.clone(),
                SecurityMode::Standard,
            );
            // Both cases grant the same single owned write path; the ceiling
            // alone decides whether the actual registered WriteTool runs.
            let policy = PermissionConfigs::from(PermissionConfig {
                default: Some(Action::Deny),
                write: Some(crate::permission::ToolPerm::Granular(
                    std::collections::HashMap::from([(
                        effect.to_string_lossy().into_owned(),
                        Action::Allow,
                    )]),
                )),
                ..Default::default()
            });
            ui.permission = Some(Arc::new(std::sync::Mutex::new(
                crate::permission::checker::PermissionChecker::new(
                    &policy,
                    SecurityMode::Standard,
                    Some(workspace.root().to_owned()),
                    None,
                )
                .unwrap(),
            )));
            let mut app = owned_app(ui);
            user(
                &mut app,
                UserEvent::Paste("write the selected owned file".into()),
            )
            .await;
            key(&mut app, KeyCode::Enter).await;
            assert_eq!(finish_turn(&mut app).await, !capped);
            assert_eq!(effect.exists(), !capped);
            if !capped {
                assert_eq!(
                    std::fs::read_to_string(&effect).unwrap(),
                    "ACTUAL_OWNED_EFFECT"
                );
            }
            assert_eq!(provider.calls().len(), if capped { 1 } else { 2 });
            let ledger = crate::spend::Ledger::new(&roots.directory.path().join("ZS_STATE_DIR"));
            let totals = ledger
                .summary_for_session(&app.ui.session.id)
                .unwrap()
                .session;
            assert_eq!(totals.records, if capped { 1 } else { 2 });
            assert!((totals.known_cost - if capped { 0.000034 } else { 0.000068 }).abs() < 1e-12);
            assert_eq!(
                app.ui.session.total_input_tokens,
                if capped { 9 } else { 18 }
            );
            assert!(app.ui.session.permission_allowlist.is_empty());
            assert!(!app.ui.session.spend_account.has_pending());
            assert!(!app.ui.session.spend_account.snapshot().tracking_error);
            drop(app);
            provider.finish().await;
        }
    }

    #[test]
    fn usd_budget_actual_midturn_compaction_reuses_account_and_prices_before_http_and_write() {
        std::thread::Builder::new()
            .name("midcompact-budget-one-mib".into())
            .stack_size(1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(crate::startup::heap_future(|| {
                        midturn_compaction_budget_matrix()
                    }))
            })
            .unwrap()
            .join()
            .unwrap();
    }

    async fn midturn_compaction_budget_matrix() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        for capped in [true, false] {
            let roots = Roots::new();
            let provider =
                Provider::new(vec!["pressure-read", "cap-write", "WRITE_COMPLETED"]).await;
            let mut cfg = provider.config();
            cfg.compact_enabled = Some(true);
            cfg.mid_turn_compact_threshold = Some(0.75);
            cfg.max_budget_usd = Some(if capped { 0.200030 } else { 1.0 });
            let cli = crate::cli::Cli::default();
            let workspace = roots.workspace();
            let effect = workspace.root().join("effect.txt");
            let read = workspace.root().join("held.txt");
            std::fs::write(&read, "owned context retained through restart").unwrap();
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new(
                "plan80",
                "owned-model",
                128_000,
                workspace.root().to_str().unwrap(),
            );
            // Known prior context explains the provider's pressure; without
            // it calibration correctly classifies 100k unexplained tokens as
            // an irreducible prompt floor and never requests compaction.
            session.add_message(MessageRole::User, &"x".repeat(80_000));
            session.add_message(MessageRole::Assistant, &"y".repeat(80_000));
            session.set_input_output_prices(Some((2.0, 4.0)));
            let id = session.id.clone();
            let mut ui = ui(
                &cli,
                &cfg,
                &mut session,
                &mut context,
                workspace.clone(),
                SecurityMode::Standard,
            );
            let policy = PermissionConfigs::from(PermissionConfig {
                default: Some(Action::Deny),
                read: Some(crate::permission::ToolPerm::Granular(
                    std::collections::HashMap::from([(
                        read.to_string_lossy().into_owned(),
                        Action::Allow,
                    )]),
                )),
                write: Some(crate::permission::ToolPerm::Granular(
                    std::collections::HashMap::from([(
                        effect.to_string_lossy().into_owned(),
                        Action::Allow,
                    )]),
                )),
                ..Default::default()
            });
            ui.permission = Some(Arc::new(std::sync::Mutex::new(
                crate::permission::checker::PermissionChecker::new(
                    &policy,
                    SecurityMode::Standard,
                    Some(workspace.root().to_owned()),
                    None,
                )
                .unwrap(),
            )));
            let mut app = owned_app(ui);
            user(
                &mut app,
                UserEvent::Paste("read held.txt then write effect.txt".into()),
            )
            .await;
            key(&mut app, KeyCode::Enter).await;
            let ledger = crate::spend::Ledger::new(&roots.directory.path().join("ZS_STATE_DIR"));
            let mut boundaries = 0;
            let mut read_succeeded = false;
            let succeeded = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                    let success = matches!(event, AgentEvent::Done { .. });
                    let terminal = success || matches!(event, AgentEvent::Error { .. });
                    if matches!(&event, AgentEvent::ToolResult { name, is_error, .. } if name.as_str() == "read" && !*is_error) {
                        read_succeeded = true;
                    }
                    if matches!(event, AgentEvent::CompactionBoundary { .. }) {
                        assert!(read_succeeded, "the real allowed read completed before restart");
                        boundaries += 1;
                        let first = ledger.summary_for_session(&id).unwrap().session;
                        assert_eq!(
                            (first.records, first.input_tokens, first.output_tokens),
                            (1, 100000, 4)
                        );
                        assert!((first.known_cost - 0.200016).abs() < 1e-12);
                        assert_eq!(provider.calls().len(), 1);
                        assert!(!app.ui.session.spend_account.has_pending());
                        assert!(!app.ui.session.spend_account.snapshot().tracking_error);
                    }
                    crate::startup::heap_future(|| app.handle_agent_event(event))
                        .await
                        .unwrap();
                    if terminal {
                        break success;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(
                boundaries,
                1,
                "the actual SDK tool result must reach the requested restart boundary; rendered={}",
                app.renderer.captured_output()
            );
            eprintln!(
                "midcompact witness: capped={capped} boundaries={boundaries} succeeded={succeeded} posts={} effect={} rows={} account_spend={:.6} session_spend={:.6}",
                provider.calls().len(),
                effect.exists(),
                ledger.summary_for_session(&id).unwrap().session.records,
                app.ui.session.spend_account.snapshot().recorded_spend(),
                app.ui.session.total_cost
            );
            assert_eq!(succeeded, !capped);
            assert_eq!(
                provider.calls().len(),
                if capped { 2 } else { 3 },
                "the admitted restarted response crosses the cap before a write or a third HTTP request"
            );
            assert_eq!(
                effect.exists(),
                !capped,
                "the same ordinary allowed write must only run with admitted spend"
            );
            if !capped {
                assert_eq!(
                    std::fs::read_to_string(&effect).unwrap(),
                    "ACTUAL_OWNED_EFFECT"
                );
            }
            for body in provider.calls() {
                assert_eq!(body["model"], "owned-model");
            }
            assert_eq!(app.ui.session.id, id);
            assert!(app.ui.session.permission_allowlist.is_empty());
            let totals = ledger.summary_for_session(&id).unwrap().session;
            assert_eq!(totals.records, if capped { 2 } else { 3 });
            assert_eq!(
                (totals.input_tokens, totals.output_tokens),
                if capped { (100009, 8) } else { (100018, 12) }
            );
            assert!((totals.known_cost - if capped { 0.200050 } else { 0.200084 }).abs() < 1e-12);
            assert!(
                (app.ui.session.spend_account.snapshot().recorded_spend() - totals.known_cost)
                    .abs()
                    < 1e-12
            );
            assert_eq!(app.ui.session.total_input_tokens, totals.input_tokens);
            assert!(!app.ui.session.spend_account.has_pending());
            assert!(!app.ui.session.spend_account.snapshot().tracking_error);
            for line in
                std::fs::read_to_string(roots.directory.path().join("ZS_STATE_DIR/usage.jsonl"))
                    .unwrap()
                    .lines()
            {
                let row: serde_json::Value = serde_json::from_str(line).unwrap();
                assert_eq!(row["session_id"], id.as_str());
                assert_eq!(row["provider"], "plan80");
                assert_eq!(row["model"], "owned-model");
                assert_eq!(row["prices"]["input"], 2.0);
                assert_eq!(row["prices"]["output"], 4.0);
                assert_eq!(row["kind"], "turn");
            }
            drop(app);
            if capped {
                provider.cancel().await;
            } else {
                provider.finish().await;
            }
        }
    }

    /// Direct consumer seam control: the existing account is already at its
    /// ceiling, so the restart must refuse HTTP even before another response.
    #[tokio::test]
    async fn usd_budget_midcompact_direct_entry_exhausted_account_refuses_http() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec!["cap-write", "WRITE_COMPLETED"]).await;
        let mut cfg = provider.config();
        cfg.max_budget_usd = Some(0.000034);
        let cli = crate::cli::Cli::default();
        let workspace = roots.workspace();
        let effect = workspace.root().join("effect.txt");
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.set_input_output_prices(Some((2.0, 4.0)));
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Yolo,
        ));
        user(
            &mut app,
            UserEvent::Paste("write the selected owned file".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        assert!(!finish_turn(&mut app).await);
        let ledger = crate::spend::Ledger::new(&roots.directory.path().join("ZS_STATE_DIR"));
        assert_eq!(
            ledger
                .summary_for_session(&app.ui.session.id)
                .unwrap()
                .session
                .records,
            1
        );
        assert_eq!(provider.calls().len(), 1);
        assert!(app.ui.session.spend_account.snapshot().exhausted());
        assert!(!app.ui.session.spend_account.has_pending());
        // This is the actual UI function with a known exhausted checkpoint;
        // it does not pretend the preceding actor emitted a pressure event.
        let core = &mut app.0;
        crate::startup::heap_future(|| {
            mid_turn_compact_and_respawn(
                0.7,
                &[],
                &mut core.renderer,
                &mut core.run,
                &mut core.ui,
                &core.slash,
            )
        })
        .await
        .unwrap();
        assert!(!finish_turn(&mut app).await);
        assert_eq!(provider.calls().len(), 1);
        let totals = ledger
            .summary_for_session(&app.ui.session.id)
            .unwrap()
            .session;
        assert_eq!(
            (totals.records, totals.input_tokens, totals.output_tokens),
            (1, 9, 4)
        );
        assert!((totals.known_cost - 0.000034).abs() < 1e-12);
        assert!(
            (app.ui.session.spend_account.snapshot().recorded_spend() - 0.000034).abs() < 1e-12
        );
        assert!(!app.ui.session.spend_account.has_pending());
        assert!(!app.ui.session.spend_account.snapshot().tracking_error);
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert!(!effect.exists());
        drop(app);
        provider.cancel().await;
    }

    #[tokio::test]
    async fn usd_budget_initial_interactive_invalid_tool_and_cancel_retain_incomplete_without_fabricated_receipt()
     {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        for response in ["unknown-tool", "hold-open"] {
            let roots = Roots::new();
            let provider = Provider::new(vec![response]).await;
            let mut cfg = provider.config();
            cfg.max_budget_usd = Some(1.0);
            let cli = crate::cli::Cli {
                no_tools: true,
                ..Default::default()
            };
            let workspace = roots.workspace();
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new(
                "plan80",
                "owned-model",
                128_000,
                workspace.root().to_str().unwrap(),
            );
            session.set_input_output_prices(Some((2.0, 4.0)));
            let mut app = owned_app(ui(
                &cli,
                &cfg,
                &mut session,
                &mut context,
                workspace,
                SecurityMode::Standard,
            ));
            begin(&mut app).await;
            if response == "hold-open" {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !app
                        .renderer
                        .captured_output()
                        .contains("STREAM_WITHOUT_FINAL")
                    {
                        let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                        assert!(!matches!(
                            event,
                            AgentEvent::Done { .. } | AgentEvent::Error { .. }
                        ));
                        crate::startup::heap_future(|| app.handle_agent_event(event))
                            .await
                            .unwrap();
                    }
                })
                .await
                .unwrap();
                app.run.pending_inputs.push_back("never dispatched".into());
                user(
                    &mut app,
                    UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                )
                .await;
                assert!(app.run.agent_rx.is_none() && app.run.main_abort.is_none());
                assert!(app.run.pending_inputs.is_empty());
            } else {
                assert!(!finish_turn(&mut app).await);
            }
            assert_eq!(provider.calls().len(), 1);
            assert!(app.ui.session.spend_account.snapshot().tracking_error);
            assert!(!app.ui.session.spend_account.has_pending());
            assert_eq!(app.ui.session.total_cost, 0.0);
            let ledger = crate::spend::Ledger::new(&roots.directory.path().join("ZS_STATE_DIR"));
            assert_eq!(
                ledger
                    .summary_for_session(&app.ui.session.id)
                    .unwrap()
                    .session
                    .records,
                0
            );
            assert!(app.ui.session.permission_allowlist.is_empty());
            assert!(app.chain.plan.pending.is_none());
            crate::session::storage::save_session(app.ui.session).unwrap();
            let saved = crate::session::storage::load_session_exact(&app.ui.session.id)
                .unwrap()
                .unwrap();
            assert!(saved.spend_account.snapshot().tracking_error);
            assert_eq!(saved.total_cost, 0.0);
            key(&mut app, KeyCode::Char('y')).await;
            assert_eq!(provider.calls().len(), 1);
            assert!(!app.run.is_running);
            drop(app);
            if response == "hold-open" {
                provider.cancel().await;
            } else {
                provider.finish().await;
            }
        }
    }

    #[tokio::test]
    async fn actual_plan_edit_requires_reapproval_and_decline_preserves_planning_and_draft() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec![PLAN]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        begin(&mut app).await;
        assert!(finish_turn(&mut app).await);
        let before = serde_json::to_vec(app.ui.session).unwrap();
        let display = app.renderer.feed().reasoning_display();
        user(
            &mut app,
            UserEvent::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        )
        .await;
        assert_eq!(app.renderer.feed().reasoning_display(), display.next());
        assert!(app.chain.plan.pending.is_some());
        assert_eq!(provider.calls().len(), 1);
        app.input.load_text("follow-up preserved in kill ring");
        key(&mut app, KeyCode::Char('e')).await;
        assert_eq!(app.input.buffer, PLAN);
        app.input
            .load_text("Edited literal plan雪\n.plan !never execute");
        key(&mut app, KeyCode::Enter).await;
        assert_eq!(provider.calls().len(), 1);
        assert!(!app.run.is_running);
        assert!(
            app.renderer
                .captured_output()
                .contains("Edited plan (awaiting approval)")
        );
        assert_eq!(
            app.chain.plan.pending.as_ref().unwrap().plan,
            "Edited literal plan雪\n.plan !never execute"
        );
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        key(&mut app, KeyCode::Char('n')).await;
        assert_eq!(provider.calls().len(), 1);
        assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
        assert_eq!(
            app.ui.permission.as_ref().unwrap().lock().unwrap().mode(),
            SecurityMode::PlanWrite
        );
        user(
            &mut app,
            UserEvent::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
        )
        .await;
        assert_eq!(app.input.buffer, "follow-up preserved in kill ring");
        assert!(app.chain.plan.pending.is_none());
        provider.finish().await;
    }

    #[tokio::test]
    async fn actual_plan_error_empty_and_caller_cancel_never_offer_execution() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        for response in ["error", " \n", "hold"] {
            let roots = Roots::new();
            let provider = Provider::new(vec![response]).await;
            let cfg = provider.config();
            let cli = crate::cli::Cli {
                no_session: true,
                no_tools: true,
                ..Default::default()
            };
            let workspace = roots.workspace();
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new(
                "plan80",
                "owned-model",
                128_000,
                workspace.root().to_str().unwrap(),
            );
            let mut app = owned_app(ui(
                &cli,
                &cfg,
                &mut session,
                &mut context,
                workspace,
                SecurityMode::Standard,
            ));
            begin(&mut app).await;
            if response == "hold" {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while provider.calls().is_empty() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                app.input.load_text("typed follow-up");
                app.run.pending_inputs.push_back("must not continue".into());
                user(
                    &mut app,
                    UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                )
                .await;
                assert!(app.run.pending_inputs.is_empty());
                assert!(app.run.agent_rx.is_none());
                assert_eq!(app.input.buffer, "add the selected flag");
                provider.cancel().await;
            } else {
                assert_eq!(finish_turn(&mut app).await, response != "error");
                assert_eq!(provider.calls().len(), 1);
                provider.finish().await;
            }
            assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
            assert!(
                !app.renderer
                    .captured_output()
                    .contains(crate::ui::plan::QUESTION)
            );
        }
    }

    #[tokio::test]
    async fn actual_plan_stale_revision_workspace_prompt_and_modified_keys_cannot_execute() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        for stale in [
            "revision",
            "workspace",
            "prompt",
            "mode",
            "session",
            "cancel",
        ] {
            let roots = Roots::new();
            let provider = Provider::new(vec![PLAN]).await;
            let cfg = provider.config();
            let cli = crate::cli::Cli {
                no_session: true,
                no_tools: true,
                ..Default::default()
            };
            let workspace = roots.workspace();
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new(
                "plan80",
                "owned-model",
                128_000,
                workspace.root().to_str().unwrap(),
            );
            let mut app = owned_app(ui(
                &cli,
                &cfg,
                &mut session,
                &mut context,
                workspace,
                SecurityMode::Standard,
            ));
            begin(&mut app).await;
            assert!(finish_turn(&mut app).await);
            let grants = serde_json::to_value(&app.ui.session.permission_allowlist).unwrap();
            for mut key_event in [
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::ALT),
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            ] {
                if key_event.modifiers.is_empty() {
                    key_event.kind = crossterm::event::KeyEventKind::Release;
                }
                user(&mut app, UserEvent::Key(key_event)).await;
                assert!(app.chain.plan.pending.is_some());
                assert_eq!(provider.calls().len(), 1);
            }
            match stale {
                "revision" => {
                    app.ui.session.messages.last_mut().unwrap().content =
                        "different same-session plan".into()
                }
                "workspace" => app.ui.workspace = roots.workspace(),
                "prompt" => {
                    app.ui
                        .context
                        .prompts
                        .insert("code".into(), "changed code prompt".into());
                }
                "mode" => {
                    user(
                        &mut app,
                        UserEvent::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
                    )
                    .await
                }
                "session" => app.ui.session.id = "different-session".into(),
                "cancel" => {}
                _ => unreachable!(),
            }
            app.run
                .pending_inputs
                .push_back("stale queued instruction".into());
            if stale == "cancel" {
                user(
                    &mut app,
                    UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                )
                .await;
            } else {
                key(&mut app, KeyCode::Char('y')).await;
            }
            assert!(app.chain.plan.pending.is_none());
            assert!(!app.run.is_running && app.run.agent_rx.is_none());
            assert!(app.run.pending_inputs.is_empty());
            assert_eq!(provider.calls().len(), 1);
            assert_eq!(
                serde_json::to_value(&app.ui.session.permission_allowlist).unwrap(),
                grants
            );
            provider.finish().await;
        }
    }

    #[tokio::test]
    async fn actual_approved_plan_permission_stop_settles_run_without_queued_followup_or_grants() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        std::fs::write(
            roots.directory.path().join("held.txt"),
            "PRIVATE_READ_MUST_NOT_COMPLETE",
        )
        .unwrap();
        let provider = Provider::new(vec![PLAN, "ask-read"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        );
        let policy = PermissionConfigs::from(PermissionConfig {
            read: Some(crate::permission::ToolPerm::Simple(Action::Ask)),
            ..Default::default()
        });
        ui.permission = Some(Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &policy,
                SecurityMode::Standard,
                Some(ui.workspace.root().to_owned()),
                None,
            )
            .unwrap(),
        )));
        let (ask, requests) = mpsc::channel(4);
        ui.ask_tx = Some(ask);
        let mut app = owned_app(ui);
        app.ask_rx = Some(requests);
        begin(&mut app).await;
        assert!(finish_turn(&mut app).await);
        key(&mut app, KeyCode::Char('y')).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            let app = &mut app.0;
            loop {
                tokio::select! {
                    Some(request) = async {app.ask_rx.as_mut().unwrap().recv().await} => {
                        let request = request.into_permission();
                        assert_eq!(request.tool,"read");
                        app.begin_permission_request(request).unwrap();
                        break;
                    }
                    Some(event) = async {app.run.agent_rx.as_mut().unwrap().recv().await} => {
                        assert!(!matches!(event,AgentEvent::Done {..}|AgentEvent::Error {..}));
                        crate::startup::heap_future(||app.handle_agent_event(event)).await.unwrap();
                    }
                }
            }
        })
        .await
        .unwrap();
        assert!(app.permission_prompt.is_some());
        assert_eq!(provider.calls().len(), 2);
        app.input.load_text("typed follow-up before stop");
        app.run
            .pending_inputs
            .push_back("must never be admitted".into());
        key(&mut app, KeyCode::Esc).await;
        assert!(app.permission_prompt.is_none());
        assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
        assert!(app.run.pending_inputs.is_empty());
        assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert!(
            !app.ui
                .session
                .messages
                .iter()
                .any(|message| message.role == MessageRole::ToolResult
                    && message.content.contains("PRIVATE_READ_MUST_NOT_COMPLETE"))
        );
        assert_eq!(
            std::fs::read_to_string(roots.directory.path().join("held.txt")).unwrap(),
            "PRIVATE_READ_MUST_NOT_COMPLETE"
        );
        assert_eq!(provider.calls().len(), 2);
        provider.finish().await;
    }

    #[tokio::test]
    async fn actual_plan_failed_local_admission_retains_literal_draft_and_replay_cannot_arm_approval()
     {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec![PLAN]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        begin(&mut app).await;
        assert!(finish_turn(&mut app).await);
        let saved = serde_json::to_vec(app.ui.session).unwrap();
        let count = app.ui.session.messages.len();
        app.ui.session.context_window = 32;
        key(&mut app, KeyCode::Char('y')).await;
        assert!(!app.run.is_running && app.run.agent_rx.is_none());
        assert!(app.input.buffer.starts_with("Execute this approved plan"));
        assert!(app.input.buffer.ends_with(PLAN));
        assert_eq!(app.ui.session.messages.len(), count);
        assert_eq!(provider.calls().len(), 1);
        assert!(
            app.renderer
                .captured_output()
                .contains("recoverable text is in the editor")
        );
        drop(app);
        session = serde_json::from_slice(&saved).unwrap();
        let workspace = roots.workspace();
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        let permission = app.ui.permission.clone();
        crate::ui::apply_prompt_mode("plan", app.ui.context, &permission);
        app.finalize_turn().await.unwrap();
        assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
        assert!(!app.run.is_running);
        assert_eq!(provider.calls().len(), 1);
        provider.finish().await;
    }

    #[cfg(feature = "goal")]
    #[tokio::test]
    async fn actual_plan_approval_does_not_take_over_unfinished_goal() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec![PLAN]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        begin(&mut app).await;
        assert!(finish_turn(&mut app).await);
        let goal = crate::extras::goal::Goal::new(
            "unfinished unrelated goal",
            vec!["retained criteria".into()],
        )
        .unwrap();
        let id = goal.id.clone();
        app.ui.session.goal_store.set(goal, false).unwrap();
        key(&mut app, KeyCode::Char('y')).await;
        assert!(app.chain.plan.pending.is_some());
        assert!(!app.run.is_running);
        assert_eq!(provider.calls().len(), 1);
        assert!(
            app.renderer
                .captured_output()
                .contains("Finish or stop the active work")
        );
        assert_eq!(app.ui.session.goal_store.snapshot().unwrap().id, id);
        key(&mut app, KeyCode::Char('n')).await;
        assert_eq!(app.ui.session.goal_store.snapshot().unwrap().id, id);
        provider.finish().await;
    }

    #[tokio::test]
    async fn actual_plan_unsettled_terminal_receiver_cannot_offer_execution() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec![PLAN]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        begin(&mut app).await;
        let done = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                if matches!(event, AgentEvent::Done { .. }) {
                    break event;
                }
                crate::startup::heap_future(|| app.handle_agent_event(event))
                    .await
                    .unwrap();
            }
        })
        .await
        .unwrap();
        // Hold the retirement boundary open after the genuine provider's Done;
        // a terminal payload alone must not confer execution authority.
        let (retained, receiver) = mpsc::channel(1);
        app.run.agent_rx = Some(receiver);
        tokio::time::pause();
        let advance = async {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(6)).await;
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(6)).await;
        };
        let (result, ()) = tokio::join!(
            crate::startup::heap_future(|| app.handle_agent_event(done)),
            advance
        );
        tokio::time::resume();
        assert!(result.unwrap_err().to_string().contains("settling"));
        assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
        assert!(
            !app.renderer
                .captured_output()
                .contains(crate::ui::plan::QUESTION)
        );
        drop(retained);
        app.run.retire(Duration::from_secs(1)).await.unwrap();
        assert_eq!(provider.calls().len(), 1);
        provider.finish().await;
    }

    #[test]
    fn actual_plan_confirmation_event_loop_runs_on_one_mib_stack() {
        std::thread::Builder::new()
            .name("plan-approval-one-mib".into())
            .stack_size(1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(crate::startup::heap_future(|| async {
                        #[cfg(feature = "hooks")]
                        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
                        let roots = Roots::new();
                        let provider = Provider::new(vec![PLAN]).await;
                        let cfg = provider.config();
                        let cli = crate::cli::Cli {
                            no_session: true,
                            no_tools: true,
                            ..Default::default()
                        };
                        let workspace = roots.workspace();
                        let mut context =
                            crate::context::load(true).for_workspace_binding(true, &workspace);
                        let mut session = Session::new(
                            "plan80",
                            "owned-model",
                            128_000,
                            workspace.root().to_str().unwrap(),
                        );
                        let mut app = owned_app(ui(
                            &cli,
                            &cfg,
                            &mut session,
                            &mut context,
                            workspace,
                            SecurityMode::Standard,
                        ));
                        let capture = app.renderer.capture_bytes();
                        let tx = app.user_tx.clone();
                        let calls = provider.requests.clone();
                        let driver = async move {
                            tx.send(UserEvent::Paste(".plan add the selected flag".into()))
                                .await
                                .unwrap();
                            tx.send(UserEvent::Key(KeyEvent::new(
                                KeyCode::Enter,
                                KeyModifiers::NONE,
                            )))
                            .await
                            .unwrap();
                            tokio::time::timeout(Duration::from_secs(5), async {
                                loop {
                                    let shown = String::from_utf8_lossy(&capture.lock().unwrap())
                                        .into_owned();
                                    if shown.contains(crate::ui::plan::QUESTION) {
                                        break;
                                    }
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await
                            .unwrap();
                            assert_eq!(calls.lock().unwrap().len(), 1);
                            tx.send(UserEvent::Key(KeyEvent::new(
                                KeyCode::Char('n'),
                                KeyModifiers::NONE,
                            )))
                            .await
                            .unwrap();
                            tokio::time::timeout(Duration::from_secs(5), async {
                                loop {
                                    if String::from_utf8_lossy(&capture.lock().unwrap())
                                        .contains("Plan not executed; still planning")
                                    {
                                        break;
                                    }
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await
                            .unwrap();
                            tx.send(UserEvent::Quit).await.unwrap();
                        };
                        let ((), result) = tokio::time::timeout(Duration::from_secs(10), async {
                            tokio::join!(driver, crate::startup::heap_future(|| app.run_inner()))
                        })
                        .await
                        .unwrap();
                        result.unwrap();
                        assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
                        assert!(!app.run.is_running && app.run.agent_rx.is_none());
                        assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
                        assert_eq!(
                            app.ui
                                .session
                                .messages
                                .iter()
                                .filter(|message| message.role == MessageRole::User)
                                .count(),
                            1
                        );
                        assert_eq!(provider.calls().len(), 1);
                        provider.finish().await;
                    }));
            })
            .unwrap()
            .join()
            .unwrap();
    }
    async fn next_question(app: &mut App<'_>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    request = app.ask_rx.as_mut().unwrap().recv() => {
                        app.begin_user_request(request.unwrap()).unwrap();
                        assert!(app.question_prompt.is_some());
                        return;
                    }
                    event = app.run.agent_rx.as_mut().unwrap().recv() => {
                        crate::startup::heap_future(|| app.handle_agent_event(event.unwrap())).await.unwrap();
                        assert!(app.run.is_running);
                    }
                }
            }
        }).await.unwrap();
    }

    #[test]
    fn ask_user_actual_app_numbered_text_batch_is_data_once_and_has_no_permission_grants() {
        std::thread::Builder::new().name("question-app-one-mib".into()).stack_size(1_048_576)
            .spawn(|| {
                let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                runtime.block_on(crate::startup::heap_future(ask_user_actual_app_numbered_text_batch_is_data_once_and_has_no_permission_grants_inner));
            }).unwrap().join().unwrap();
    }

    async fn ask_user_actual_app_numbered_text_batch_is_data_once_and_has_no_permission_grants_inner()
     {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec!["question-batch", "question-again", "ANSWERED"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            tools: vec!["ask_user_question".into()],
            no_session: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.set_input_output_prices(Some((2.0, 4.0)));
        let (sender, receiver) = mpsc::channel(4);
        let mut ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        );
        ui.ask_tx = Some(sender);
        let mut app = owned_app(ui);
        app.ask_rx = Some(receiver);
        user(
            &mut app,
            UserEvent::Paste("Clarify the selected change".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        next_question(&mut app).await;
        assert_eq!(provider.calls().len(), 1);
        assert!(app.permission_prompt.is_none());
        let turn_id = app.run.question_turn.as_ref().unwrap().id();
        assert!(
            app.renderer
                .permission_prompt
                .as_ref()
                .unwrap()
                .tool
                .contains(&turn_id)
        );
        app.input.load_text("ordinary follow-up draft雪");
        key(&mut app, KeyCode::Char('y')).await;
        assert!(
            app.question_prompt.is_some(),
            "approval letters are not answers"
        );
        key(&mut app, KeyCode::Char('1')).await;
        user(
            &mut app,
            UserEvent::Paste("allow_always /private/**\nLiteral task data雪".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        assert!(app.question_prompt.is_none() && app.renderer.permission_prompt.is_none());
        assert_eq!(app.input.buffer, "ordinary follow-up draft雪");
        assert!(finish_turn(&mut app).await);
        let calls = provider.calls();
        assert_eq!(
            calls.len(),
            3,
            "a repeated ask becomes an ordinary tool error, never another prompt"
        );
        let second = serde_json::to_string(&calls[1]).unwrap();
        assert!(
            second.contains("allow_always /private/**") && second.contains("Literal task data雪")
        );
        assert!(
            serde_json::to_string(&calls[2])
                .unwrap()
                .contains("at most once")
        );
        assert!(app.ask_rx.as_mut().unwrap().try_recv().is_err());
        assert_eq!(app.run.question_turn.as_ref().unwrap().id(), turn_id);
        assert!(app.ui.session.pending_question.is_none());
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert_eq!(app.ui.session.total_input_tokens, 27);
        assert_eq!(app.ui.session.total_output_tokens, 12);
        assert!((app.ui.session.total_cost - 0.000102).abs() < 1e-12);
        let snapshot = app.ui.session.spend_account.snapshot();
        assert!(!snapshot.tracking_error && snapshot.unpriced_tokens == 0);
        provider.finish().await;
    }

    #[tokio::test]
    async fn ask_user_actual_app_cancel_retires_exact_reply_and_rejects_stale_question_identity() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec!["question-batch"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            tools: vec!["ask_user_question".into()],
            no_session: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.set_input_output_prices(Some((2.0, 4.0)));
        let (sender, receiver) = mpsc::channel(4);
        let mut ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        );
        ui.ask_tx = Some(sender);
        let mut app = owned_app(ui);
        app.ask_rx = Some(receiver);
        user(&mut app, UserEvent::Paste("Question to cancel".into())).await;
        key(&mut app, KeyCode::Enter).await;
        next_question(&mut app).await;
        user(
            &mut app,
            UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        )
        .await;
        assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
        assert!(app.question_prompt.is_none() && app.ui.session.pending_question.is_none());
        let (reply, answer) = tokio::sync::oneshot::channel();
        let request = crate::permission::ask::QuestionRequest {
            id: uuid::Uuid::new_v4().to_string(),
            questions: crate::question::QuestionBatch(vec![crate::question::Question {
                question: "STALE".into(),
                options: vec![],
                allow_free_text: true,
            }]),
            #[cfg(feature = "acp")]
            tool_call_id: Some("stale-call".into()),
            reply,
        };
        app.begin_user_request(crate::permission::ask::UserRequest::Question(request))
            .unwrap();
        assert!(matches!(
            answer.await.unwrap(),
            crate::question::QuestionAnswer::Cancelled
        ));
        assert!(app.question_prompt.is_none());
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert_eq!(provider.calls().len(), 1);
        assert_eq!(app.ui.session.total_input_tokens, 9);
        assert_eq!(app.ui.session.total_output_tokens, 4);
        assert!((app.ui.session.total_cost - 0.000034).abs() < 1e-12);
        provider.finish().await;
    }
    #[tokio::test]
    async fn ask_user_actual_app_fail_parks_queue_and_clear_retires_pending_data_without_grants() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec!["question-batch"]).await;
        let mut cfg = provider.config();
        cfg.headless_questions = Some(crate::config::HeadlessQuestions::Fail);
        let cli = crate::cli::Cli {
            tools: vec!["ask_user_question".into()],
            no_session: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.set_input_output_prices(Some((2.0, 4.0)));
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        // A deliberately absent interactive transport exercises the same typed
        // terminal consumed when an ACP client asks to answer in a new message.
        assert!(app.ui.ask_tx.is_none());
        user(
            &mut app,
            UserEvent::Paste("Clarify before continuing".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        app.input.load_text("owner follow-up draft雪");
        app.run
            .pending_inputs
            .push_back("queued task must stay parked".into());
        assert!(!finish_turn(&mut app).await);
        assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
        assert!(app.question_prompt.is_none() && app.permission_prompt.is_none());
        assert_eq!(app.input.buffer, "owner follow-up draft雪");
        let pending = app.ui.session.pending_question.clone().unwrap();
        assert_eq!(pending.id, app.run.question_turn.as_ref().unwrap().id());
        assert_eq!(pending.questions.0.len(), 2);
        assert!(pending.answers.is_empty());
        assert!(
            app.renderer
                .captured_output()
                .contains("queued prompts remain parked")
        );
        app.finalize_turn().await.unwrap();
        assert_eq!(
            app.run.pending_inputs.front().map(String::as_str),
            Some("queued task must stay parked")
        );
        assert_eq!(provider.calls().len(), 1);
        assert_eq!(
            (
                app.ui.session.total_input_tokens,
                app.ui.session.total_output_tokens
            ),
            (9, 4)
        );
        assert!((app.ui.session.total_cost - 0.000034).abs() < 1e-12);
        assert!(!app.ui.session.spend_account.has_pending());
        assert!(!app.ui.session.spend_account.snapshot().tracking_error);
        assert!(app.ui.session.permission_allowlist.is_empty());
        let decoded: Session =
            serde_json::from_slice(&serde_json::to_vec(app.ui.session).unwrap()).unwrap();
        assert_eq!(decoded.pending_question.unwrap().id, pending.id);
        // The owner removes parked queued input before clearing the transcript.
        app.run.pending_inputs.clear();
        app.run_slash_command("/clear").await.unwrap();
        assert!(app.ui.session.pending_question.is_none() && app.run.question_turn.is_none());
        assert!(app.ui.session.messages.is_empty());
        assert!((app.ui.session.total_cost - 0.000034).abs() < 1e-12);
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert_eq!(provider.calls().len(), 1);
        provider.finish().await;
    }

    #[cfg(feature = "subagents")]
    #[tokio::test]
    async fn ask_user_native_auxiliary_clears_even_an_explicit_main_slot_without_consuming_it() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec!["question-batch", "CHILD_DONE"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            tools: vec!["ask_user_question".into()],
            no_session: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        app.ensure_agent().await;
        let turn = crate::question::QuestionTurn::default();
        let candidate = app
            .run
            .agent
            .as_ref()
            .unwrap()
            .clone()
            .with_question_turn(Some(turn.clone()));
        let usage = crate::agent::runner::SharedUsageLedger::default();
        let observed = usage.clone();
        crate::question::scope(Some(turn.clone()), async {
            let result = crate::startup::heap_future(|| {
                candidate.run_subagent("Child intent", 3, None, &cfg.retry, usage)
            })
            .await;
            assert_eq!(result.response.unwrap(), "CHILD_DONE");
            assert_eq!(
                crate::question::QuestionTurn::current().unwrap().id(),
                turn.id()
            );
        })
        .await;
        assert!(
            turn.claim().is_ok(),
            "auxiliary transport cannot consume the parent's once slot"
        );
        assert!(turn.required().is_none());
        assert_eq!(observed.total().input_tokens, 18);
        assert_eq!(observed.total().output_tokens, 8);
        let calls = provider.calls();
        assert_eq!(calls.len(), 2);
        assert!(
            serde_json::to_string(&calls[1])
                .unwrap()
                .contains("available only in the main user turn")
        );
        assert!(
            app.ui.session.pending_question.is_none()
                && app.ui.session.permission_allowlist.is_empty()
        );
        provider.finish().await;
    }

    #[tokio::test]
    async fn ask_user_actual_pressure_restart_preserves_the_preclaimed_main_slot_and_budget_profile()
     {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec![
            "pressure-read",
            "question-again",
            "FINISHED_AFTER_RESTART",
        ])
        .await;
        let mut cfg = provider.config();
        cfg.compact_enabled = Some(true);
        cfg.mid_turn_compact_threshold = Some(0.75);
        let cli = crate::cli::Cli {
            tools: vec!["read".into(), "ask_user_question".into()],
            ..Default::default()
        };
        let workspace = roots.workspace();
        let read = workspace.root().join("held.txt");
        std::fs::write(&read, "owned allowed read").unwrap();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.add_message(MessageRole::User, &"x".repeat(80_000));
        session.add_message(MessageRole::Assistant, &"y".repeat(80_000));
        session.set_input_output_prices(Some((2.0, 4.0)));
        let id = session.id.clone();
        let mut ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace.clone(),
            SecurityMode::Standard,
        );
        ui.permission = Some(Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &PermissionConfigs::from(PermissionConfig {
                    default: Some(Action::Deny),
                    read: Some(crate::permission::ToolPerm::Granular(
                        std::collections::HashMap::from([(
                            read.to_string_lossy().into_owned(),
                            Action::Allow,
                        )]),
                    )),
                    ..Default::default()
                }),
                SecurityMode::Standard,
                Some(workspace.root().to_path_buf()),
                None,
            )
            .unwrap(),
        )));
        let mut app = owned_app(ui);
        user(
            &mut app,
            UserEvent::Paste("Continue this ordinary turn".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        let captured = app.run.question_turn.as_ref().unwrap().clone();
        captured.claim().unwrap();
        let mut boundaries = 0;
        let mut rejected_again = false;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                let done = matches!(event, AgentEvent::Done { .. });
                assert!(!matches!(
                    event,
                    AgentEvent::Error { .. } | AgentEvent::AwaitingUser { .. }
                ));
                if matches!(event, AgentEvent::CompactionBoundary { .. }) {
                    boundaries += 1;
                }
                if let AgentEvent::ToolResult {
                    name,
                    output,
                    is_error,
                    ..
                } = &event
                    && name.as_str() == "ask_user_question"
                {
                    assert!(*is_error);
                    assert!(output.contains("at most once"));
                    rejected_again = true;
                }
                crate::startup::heap_future(|| app.handle_agent_event(event))
                    .await
                    .unwrap();
                assert_eq!(app.run.question_turn.as_ref().unwrap().id(), captured.id());
                if done {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(boundaries, 1);
        assert!(rejected_again);
        assert_eq!(provider.calls().len(), 3);
        let persisted = crate::spend::Ledger::new(&roots.directory.path().join("ZS_STATE_DIR"))
            .summary_for_session(&id)
            .unwrap()
            .session;
        assert_eq!(persisted.records, 3);
        assert_eq!(
            (persisted.input_tokens, persisted.output_tokens),
            (100018, 12)
        );
        assert!((persisted.known_cost - 0.200084).abs() < 1e-12);
        assert!(
            !app.ui.session.spend_account.has_pending()
                && !app.ui.session.spend_account.snapshot().tracking_error
        );
        assert!(
            app.ui.session.pending_question.is_none()
                && app.ui.session.permission_allowlist.is_empty()
        );
        provider.finish().await;
    }
    #[test]
    fn model_listing48_actual_refresh_and_picker_keep_session_and_do_not_call_model_on_one_mib_stack()
     {
        std::thread::Builder::new().name("model48-app-one-mib".into()).stack_size(1024*1024).spawn(|| {
            let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(crate::startup::heap_future(|| async {
                let roots=Roots::new(); let workspace=roots.workspace();
                let provider=Provider::new(Vec::new()).await; let cfg=provider.config();
                let cli=crate::cli::Cli { no_session:true,no_context_files:true,no_tools:true,api_key:Some("unused-local-key".into()),..Default::default() };
                let mut files=crate::context::load(true).for_workspace_binding(true,&workspace);
                let mut session=Session::new("anthropic","owned-model",128000,"model-listing48");
                session.set_input_output_prices(Some((2.0,3.0)));
                let mut app=owned_app(ui(&cli,&cfg,&mut session,&mut files,workspace,SecurityMode::Standard));
                let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                app.ui.client=crate::provider::AnyClient::Anthropic(rig::providers::anthropic::Client::builder()
                    .api_key("CAPTURED_LISTING_KEY").base_url(format!("http://{}",listener.local_addr().unwrap()))
                    .http_client(crate::provider_http::ProviderHttpClient::default()).build().unwrap());
                let before=serde_json::to_vec(app.ui.session).unwrap();
                let operator=async {
                    app.input.buffer="/models refresh".into(); app.input.cursor=app.input.buffer.len();
                    user(&mut app,UserEvent::Key(KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE))).await;
                    app.input.buffer="/models".into(); app.input.cursor=app.input.buffer.len();
                    user(&mut app,UserEvent::Key(KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE))).await;
                };
                let server=async {
                    let (mut socket,_)=listener.accept().await.unwrap();
                    let mut bytes=Vec::new();
                    while !bytes.windows(4).any(|v| v==b"\r\n\r\n") { let mut chunk=[0;1024];let n=socket.read(&mut chunk).await.unwrap();assert!(n>0);bytes.extend_from_slice(&chunk[..n]);assert!(bytes.len()<16384); }
                    assert!(String::from_utf8(bytes).unwrap().starts_with("GET /v1/models?limit=1000 "));
                    let body=r#"{"data":[{"id":"claude-opus-5-5","display_name":"Opus 雪","max_input_tokens":200000},{"id":"new-unpriced-model","max_input_tokens":64000}],"has_more":false}"#;
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                };
                tokio::time::timeout(Duration::from_secs(5),async {tokio::join!(operator,server);}).await.unwrap();
                assert_eq!(serde_json::to_vec(app.ui.session).unwrap(),before);
                let captured=app.renderer.captured_output(); assert!(captured.contains("model list refreshed"));assert!(captured.contains("claude-opus-5-5"));assert!(captured.contains("200k ctx"));assert!(captured.contains("(no price known)"));
                let ids=crate::ui::slash::cached_model_ids("anthropic",false,&app.ui.client);assert!(ids.contains(&"claude-opus-5-5".into()));
                assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
                assert!(provider.calls().is_empty());
                assert!(tokio::time::timeout(Duration::from_millis(20),listener.accept()).await.is_err());
                provider.finish().await;
            }));
        }).unwrap().join().unwrap();
    }
    #[tokio::test]
    async fn model_listing48_actual_selection_uses_completed_metadata_and_retains_local_pin() {
        for pin in [None, Some(123456)] {
            let roots = Roots::new();
            let workspace = roots.workspace();
            let provider = Provider::new(Vec::new()).await;
            let mut cfg = provider.config();
            cfg.context_window = pin;
            let cli = crate::cli::Cli {
                no_session: true,
                no_context_files: true,
                no_tools: true,
                api_key: Some("unused-local-key".into()),
                ..Default::default()
            };
            let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session =
                Session::new("openrouter", "old-owned-model", 128000, "model-selection48");
            let mut app = owned_app(ui(
                &cli,
                &cfg,
                &mut session,
                &mut files,
                workspace,
                SecurityMode::Standard,
            ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            app.ui.client = crate::provider::AnyClient::OpenRouter(
                rig::providers::openrouter::Client::builder()
                    .api_key("CAPTURED_SELECTION_KEY")
                    .base_url(format!("http://{}", listener.local_addr().unwrap()))
                    .http_client(crate::provider_http::ProviderHttpClient::default())
                    .build()
                    .unwrap(),
            );
            let operator = async {
                app.input.buffer = "/model selection48-live".into();
                app.input.cursor = app.input.buffer.len();
                user(
                    &mut app,
                    UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                )
                .await;
            };
            let server = async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|v| v == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    assert!(bytes.len() < 16384);
                }
                assert!(
                    String::from_utf8(bytes)
                        .unwrap()
                        .starts_with("GET /models ")
                );
                let body = r#"{"data":[{"id":"selection48-live","context_length":240000,"pricing":{"prompt":"0.000007","completion":"0.000011"}}]}"#;
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(operator, server);
            })
            .await
            .unwrap();
            assert_eq!(app.ui.session.model.as_str(), "selection48-live");
            assert_eq!(app.ui.session.context_window, pin.unwrap_or(240000));
            assert!((app.ui.session.input_token_cost - 7.0).abs() < 1e-12);
            assert!((app.ui.session.output_token_cost - 11.0).abs() < 1e-12);
            assert!(app.ui.session.messages.is_empty());
            assert!(provider.calls().is_empty());
            assert!(
                tokio::time::timeout(Duration::from_millis(20), listener.accept())
                    .await
                    .is_err()
            );
            provider.finish().await;
        }
    }
}
