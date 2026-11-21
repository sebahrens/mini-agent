#[path = "cron_app.rs"]
mod cron_app;
#[path = "job_wake_app.rs"]
mod job_wake_app;
#[cfg(feature = "mcp")]
#[path = "mcp_admin_app.rs"]
mod mcp_admin_app;
#[cfg(feature = "export")]
#[path = "share_app.rs"]
mod share_app;

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
use crate::config::keybindings::{KeyAction, KeyBindings};
use crate::event::{AgentEvent, BtwEvent, UserEvent};
#[cfg(feature = "mcp")]
use crate::extras::mcp::{McpClientManager, McpNoticeLevel};
use crate::provider::AnyAgent;
use crate::sandbox::CommandCancellation;
use crate::sandbox::{CommandLimits, CommandStatus, SupportCommandAudit, SupportCommandLimits};
use crate::session::{GitStatus, Session};
use crate::ui::event_handler;
use crate::ui::events::{render_session, sanitize_output};
use crate::ui::input::{InputEditor, Picker};
use crate::ui::permission_handler::{PermissionEvent, PermissionState};
use crate::ui::pickers::rewind::{RewindBinding, RewindMode, RewindOutcome};
use crate::ui::question_handler::{QuestionEvent, QuestionState};
use crate::ui::renderer::{
    self as renderer_mod, ChainPrompt, ClipboardCopyOutcome, Renderer, SelectionPoint,
    copy_to_clipboard, read_from_clipboard,
};
use crate::ui::slash::{C_RESULT, DeferredSlash, apply_prompt_model, handle_compress};
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

/// `--no-color`, or a non-empty `NO_COLOR` in the environment (no-color.org):
/// either one drops syntax colours so spans are not computed for nothing.
fn monochrome_requested(flag: bool) -> bool {
    monochrome_from(flag, std::env::var_os("NO_COLOR").as_deref())
}

fn monochrome_from(flag: bool, env: Option<&std::ffi::OsStr>) -> bool {
    flag || env.is_some_and(|value| !value.is_empty())
}

#[cfg(feature = "mcp")]
fn mcp_notice_color(level: McpNoticeLevel) -> Color {
    match level {
        McpNoticeLevel::Info => C_RESULT,
        McpNoticeLevel::Error => C_ERROR,
    }
}

#[cfg(test)]
#[path = "app_tests/monochrome_env_tests.rs"]
mod monochrome_env_tests;

#[cfg(all(test, feature = "mcp"))]
mod mcp_notice_tests {
    use super::*;

    #[test]
    fn informational_notices_use_system_style_and_failures_use_error_style() {
        assert_eq!(mcp_notice_color(McpNoticeLevel::Info), C_RESULT);
        assert_eq!(mcp_notice_color(McpNoticeLevel::Error), C_ERROR);
    }
}

// Each value only carries one received event from the channel to its handler;
// boxing it would add an allocation per event.
#[allow(clippy::large_enum_variant)]
enum AgentStreamItem {
    Event(AgentEvent),
    Closed,
}

async fn recv_agent_stream_item(
    receiver: &mut Option<mpsc::Receiver<AgentEvent>>,
) -> Option<AgentStreamItem> {
    let receiver = receiver.as_mut()?;
    Some(match receiver.recv().await {
        Some(event) => AgentStreamItem::Event(event),
        None => AgentStreamItem::Closed,
    })
}

/// Echo a submitted prompt as `> line` rows plus a blank separator, painted
/// as one frame instead of one chat repaint per line. The frame closes before
/// the caller awaits anything, so the echo is on screen while the turn starts.
fn echo_submission(renderer: &mut Renderer, text: &str) -> io::Result<()> {
    renderer.frame(|renderer| {
        for line in text.lines() {
            renderer.write_line(&format!("> {}", sanitize_output(line)), Color::Green)?;
        }
        renderer.write_line("", Color::White)
    })
}

#[cfg(test)]
#[path = "app_tests/echo_frame_tests.rs"]
mod echo_frame_tests;

/// Feed blocks laid out per idle step after a tail-first chat paint: small
/// enough that a step (markdown + highlighting in a debug build) stays a few
/// milliseconds, so keys and stream events are never held behind it.
const FEED_LAYOUT_STEP_BLOCKS: usize = 16;
/// Pause between idle layout steps; other ready events win the select.
const FEED_LAYOUT_STEP_DELAY: Duration = Duration::from_millis(1);

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
#[path = "app_tests/test_support.rs"]
pub(super) mod test_support;

#[cfg(test)]
#[path = "app_tests/dot_command_tests.rs"]
mod dot_command_tests;

#[cfg(test)]
#[path = "app_tests/btw_pricing_tests.rs"]
mod btw_pricing_tests;

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
    /// The session directory the branch was read from.
    branch: Option<compact_str::CompactString>,
    /// `None` when the repository fingerprint was unchanged and the status
    /// spawn was skipped (or the statusline does not show status).
    snapshot: Option<Option<GitStatus>>,
    fingerprint: Option<crate::ui::git_poll::GitFingerprint>,
}

/// Apply a finished refresh. `None` when it describes a workspace that is no
/// longer current; otherwise whether anything shown changed.
fn apply_git_status_refresh(
    session: &mut Session,
    workspace_root: &Path,
    refresh: &GitStatusRefresh,
) -> Option<bool> {
    if refresh.working_dir != workspace_root {
        return None;
    }
    let mut changed = false;
    if session.git_branch != refresh.branch {
        session.git_branch.clone_from(&refresh.branch);
        changed = true;
    }
    if let Some(snapshot) = &refresh.snapshot
        && session.git_status != *snapshot
    {
        session.git_status.clone_from(snapshot);
        changed = true;
    }
    Some(changed)
}

/// Event-loop scheduling state owned by `run_inner`.
#[derive(Default)]
struct EventLoopState {
    /// Persistent stream-tail / spinner deadline while a run is active.
    running_tick: crate::ui::event_handler::RunningTick,
    git_poll: crate::ui::git_poll::GitPoll,
    /// While a coalesced batch of user events is applied, `refresh()` only
    /// records that a frame is owed; the batch paints once at its end.
    defer_paint: bool,
    paint_owed: bool,
    /// The model last handed to the model picker, so unchanged frames do not
    /// allocate it again.
    synced_model: Option<compact_str::CompactString>,
    /// The listing route (custom flag and key) whose ids the model picker
    /// holds, so a slash command that changed neither re-clones no ids.
    synced_route: Option<(bool, Option<crate::provider_listing::ListingKey>)>,
    /// Last MCP prompt-source revalidation; the expensive check runs at most
    /// once per second while the cheap reply/owner checks run every pass.
    last_source_revalidation: Option<std::time::Instant>,
    #[cfg(test)]
    frames: usize,
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

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardShortcut {
    CopySelection,
    Paste,
}

#[cfg(test)]
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

#[cfg(test)]
thread_local! {
    /// Links `App::open_link` was asked to open in tests (nothing is spawned).
    static OPENED_LINKS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl App<'_> {
    /// Open `url` in the browser without blocking the event loop.
    fn open_link(&self, url: String) {
        #[cfg(test)]
        {
            let _ = renderer_mod::open_url;
            OPENED_LINKS.with(|opened| opened.borrow_mut().push(url));
        }
        #[cfg(not(test))]
        {
            let tx = self.user_tx.clone();
            tokio::spawn(async move {
                if let Err(error) = renderer_mod::open_url(&url).await {
                    let _ = tx.send(UserEvent::LinkOpenFailed(error.to_string())).await;
                }
            });
        }
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
#[path = "app_tests/mouse_selection_tests.rs"]
mod mouse_selection_tests;

/// The lazygit shortcut, Ctrl+O. It used to be Ctrl+H, but many terminals
/// send `^H` for Backspace and crossterm cannot tell the two apart, so Ctrl+H
/// stays backspace in the editor and pickers.
#[cfg(test)]
fn is_lazygit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('o' | 'O'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

/// Ctrl+T folds or unfolds the latest tool result. Ctrl+O is taken by
/// lazygit; Ctrl+T is unbound in the editor and pickers.
fn is_tool_output_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('t' | 'T'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

/// Ctrl+Z (no other modifier): the terminal is in raw mode, where ISIG is off,
/// so the shell's job-control key arrives as an ordinary key event.
#[cfg(unix)]
fn is_suspend_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('z' | 'Z'))
        && key.modifiers == KeyModifiers::CONTROL
        && key.kind == crossterm::event::KeyEventKind::Press
}

/// Ctrl+Z suspends only when the user has not bound it (an `undo = ["ctrl+z"]`
/// binding wins) and no modal owns the keyboard: a permission or question
/// prompt, a chain prompt or an open picker never hands the terminal back.
#[cfg(unix)]
fn suspend_applies(key: KeyEvent, bound: bool, modal: bool) -> bool {
    is_suspend_key(key) && !bound && !modal
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
#[path = "app_tests/home_end_tests.rs"]
mod home_end_tests;

#[cfg(all(test, unix))]
mod suspend_key_tests {
    use super::*;

    #[test]
    fn ctrl_z_suspends_only_when_unbound_and_no_modal_owns_the_keyboard() {
        let ctrl_z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert!(suspend_applies(ctrl_z, false, false));
        // An undo = ["ctrl+z"] binding wins; a permission prompt or picker is inert.
        assert!(!suspend_applies(ctrl_z, true, false));
        assert!(!suspend_applies(ctrl_z, false, true));
        for other in [
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            KeyEvent::new_with_kind(
                KeyCode::Char('z'),
                KeyModifiers::CONTROL,
                crossterm::event::KeyEventKind::Release,
            ),
        ] {
            assert!(!suspend_applies(other, false, false), "{other:?}");
        }
    }

    #[test]
    fn a_user_undo_binding_on_ctrl_z_is_reconfigured_and_keeps_the_key() {
        let ctrl_z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
        let default = KeyBindings::default();
        assert!(default.action(ctrl_z, false).is_none());
        assert!(!default.is_reconfigured(ctrl_z));
        let mut keys = std::collections::BTreeMap::new();
        keys.insert("undo".to_string(), vec!["ctrl+z".to_string()]);
        let bound = KeyBindings::from_overrides(Some(&keys)).unwrap();
        assert!(bound.is_reconfigured(ctrl_z));
    }
}

#[cfg(test)]
#[path = "app_tests/tool_output_key_tests.rs"]
mod tool_output_key_tests;

#[cfg(test)]
#[path = "app_tests/lazygit_key_tests.rs"]
mod lazygit_key_tests;

#[cfg(test)]
#[path = "app_tests/agent_stream_tests.rs"]
mod agent_stream_tests;

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
#[path = "app_tests/git_status_refresh_tests.rs"]
mod git_status_refresh_tests;

/// What Ctrl+D (and legacy callers) do when nothing is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleInterrupt {
    /// Clear a draft (recoverable with Ctrl+Y).
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

/// The typed first-request tool marker for an `@agent` mention turn: the
/// `task` tool under `agent_mention_tool_choice = "force"`, otherwise none
/// (the routing directive alone is the hint).
#[cfg(feature = "subagents")]
fn mention_first_request_tool(cfg: &crate::config::Config) -> Option<&'static str> {
    match cfg.resolve_agent_mention_tool_choice() {
        crate::config::AgentMentionToolChoice::Force => {
            Some(crate::agent::first_tool::TASK_TOOL_NAME)
        }
        crate::config::AgentMentionToolChoice::Hint => None,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlCAction {
    Interrupt(InterruptTarget),
    ClearDraft,
    ArmExit,
    Exit,
}

fn ctrl_c_action(exit_armed: bool, target: InterruptTarget, draft_empty: bool) -> CtrlCAction {
    if target != InterruptTarget::Exit {
        CtrlCAction::Interrupt(target)
    } else if exit_armed {
        CtrlCAction::Exit
    } else if draft_empty {
        CtrlCAction::ArmExit
    } else {
        CtrlCAction::ClearDraft
    }
}

fn ctrl_c_feedback(action: CtrlCAction) -> &'static str {
    match action {
        CtrlCAction::Interrupt(_) => "interrupt requested; press Ctrl-C again to exit",
        CtrlCAction::ClearDraft => "draft cleared; press Ctrl-C again to exit",
        CtrlCAction::ArmExit => "press Ctrl-C again to exit",
        CtrlCAction::Exit => "exit requested; shutting down...",
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
    /// Session-local `/cron` schedules; the event loop only reads deadlines.
    cron: super::cron::CronStore,
    /// Payload of the scheduler fire whose turn is running, so an interrupt
    /// does not load it into the user's editor.
    cron_turn: Option<String>,
    /// The self-paced `/cron` fire whose turn is running; settled (tool
    /// removed, next wakeup applied or entry retired) when that turn ends.
    cron_dynamic: Option<cron_app::DynamicCronTurn>,
    /// The latest collapsed noop line, which the next consecutive noop fire
    /// of the same entry updates in place.
    cron_noop: Option<cron_app::NoopLine>,

    renderer: Renderer,
    input: InputEditor,
    /// Event-loop scheduling: running tick, Git polling, batched painting.
    loop_state: EventLoopState,
    rewind_escape: Option<std::time::Instant>,
    git_status_tx: mpsc::Sender<GitStatusRefresh>,
    git_status_rx: mpsc::Receiver<GitStatusRefresh>,
    /// A command-backed status segment's text changed (repaint wanted).
    command_segment_tx: mpsc::Sender<()>,
    command_segment_rx: mpsc::Receiver<()>,
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
    deferred_user_events: std::collections::VecDeque<UserEvent>,
    /// Custom slash templates for completion, re-read only when a template
    /// layer directory changed.
    #[cfg(feature = "subagents")]
    background_requests: std::collections::VecDeque<crate::permission::ask::UserRequest>,
    running: Arc<AtomicBool>,
    event_handle: Option<super::InputReader>,
    prebuild: Option<super::prebuild::AgentPrebuild>,
    terminal_guard: TerminalGuard,
    /// Last model-cache generation handed to the model picker.
    #[cfg(feature = "mcp")]
    mcp_admin: Option<Box<crate::ui::mcp_admin::State>>,
    model_cache_generation: u64,
    /// One Ctrl-C cancels or clears and arms exit; only the next consecutive
    /// Ctrl-C exits the interactive process.
    ctrl_c_exit_armed: bool,
    /// SessionStart hook work deferred past the first frame (see
    /// [`App::finish_startup_work`]).
    session_start_task: Option<SessionStartTask>,
    /// Startup skill discovery for the "/" picker, applied when it lands.
    skill_commands: Option<PendingSkillCommands>,
    /// Opt-in update check started in the background at startup.
    update_notice: Option<tokio::sync::oneshot::Receiver<String>>,
}

struct PendingSkillCommands {
    /// Discovery ran for this binding; a worktree rebind discards it.
    workspace: Arc<crate::paths::WorkspaceBinding>,
    /// Picker entries and how many packages the scan rejected.
    rx: tokio::sync::oneshot::Receiver<(Vec<(String, String)>, usize)>,
    /// Joined (bounded) at teardown so discovery never outlives the App.
    task: tokio::task::JoinHandle<()>,
    /// A later explicit listing already replaced the picker entries.
    stale: bool,
}

#[cfg(feature = "hooks")]
type SessionStartOutput = crate::extras::hooks::OwnedSessionStartContext;
#[cfg(not(feature = "hooks"))]
type SessionStartOutput = std::convert::Infallible;
type SessionStartTask = tokio::task::JoinHandle<SessionStartOutput>;

/// `--worktree` / `--parallel` rebuild the agent inside `App::new`.
fn worktree_startup_requested(cli: &crate::cli::Cli) -> bool {
    #[cfg(feature = "git-worktree")]
    {
        cli.worktree.is_some() || cli.parallel
    }
    #[cfg(not(feature = "git-worktree"))]
    {
        let _ = cli;
        false
    }
}

/// Plain editing, paste, scroll and resize stay responsive while deferred
/// startup work runs; everything else (submission, commands, keybound
/// actions, quit) joins it first.
fn defers_startup_work(event: &UserEvent) -> bool {
    match event {
        UserEvent::Paste(_)
        | UserEvent::ScrollUp
        | UserEvent::ScrollDown
        | UserEvent::ScrollPageUp
        | UserEvent::ScrollPageDown
        | UserEvent::Resize => true,
        UserEvent::Key(key) => {
            key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                && matches!(
                    key.code,
                    KeyCode::Char(_)
                        | KeyCode::Backspace
                        | KeyCode::Delete
                        | KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Home
                        | KeyCode::End
                )
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "app_tests/startup_work_tests.rs"]
mod startup_work_tests;

#[cfg(test)]
#[path = "app_tests/reasoning_display_tests.rs"]
mod reasoning_display_tests;

impl<'a> App<'a> {
    pub(crate) async fn new(
        mut ui: UiContext<'a>,
        agent: Option<AnyAgent>,
        ask_rx: Option<mpsc::Receiver<crate::permission::ask::UserRequest>>,
        auto_trigger_msg: Option<String>,
        #[cfg(feature = "advisor")] handoff_rx: Option<crate::extras::advisor::HandoffReceiver>,
        #[cfg(feature = "hooks")] mut session_start_task: Option<
            tokio::task::JoinHandle<crate::extras::hooks::OwnedSessionStartContext>,
        >,
    ) -> anyhow::Result<Self> {
        #[cfg(all(feature = "hooks", feature = "mcp"))]
        super::bind_current_mcp_hook_peers(ui.mcp_manager.as_ref());
        let keybindings =
            KeyBindings::from_overrides(ui.cfg.keys.as_ref()).map_err(anyhow::Error::msg)?;
        let terminal_guard = TerminalGuard::new(crate::ui::terminal::TerminalOptions {
            mouse_capture: ui.cfg.resolve_mouse_capture(),
            title_status: ui.cfg.resolve_terminal_title(),
        })?;

        // No input thread exists yet: retain every unrelated decoded event for
        // its sole owner instead of flushing or racing raw terminal input.
        // The query is sent now and its reply collected after the renderer,
        // input editor and history are built, overlapping the round trip.
        let background_query = crate::ui::terminal::start_background_query(!ui.cli.no_color);

        ui.session.show_cost_always = ui.cfg.resolve_show_cost_always();
        crate::ui::statusline::init(ui.cfg);

        ui.session.refresh_git_branch();

        let mut renderer = Renderer::new()?;
        renderer.set_statusline_height(crate::ui::statusline::line_count());
        renderer.set_monochrome(monochrome_requested(ui.cli.no_color));
        renderer.set_title_status(ui.cfg.resolve_terminal_title());
        renderer.set_activity_signals(crate::ui::terminal::ActivitySignals {
            notify: ui.cfg.resolve_terminal_notify(),
            prompt_marks: ui.cfg.resolve_terminal_prompt_marks(),
            min_turn_secs: ui.cfg.resolve_terminal_notify_min_secs(),
            ..Default::default()
        });
        renderer.set_chat_margin(ui.cfg.resolve_chat_left_margin());

        let mut input = InputEditor::new();
        input.keybindings = keybindings;
        input.set_vim_enabled(ui.cfg.vim_mode.unwrap_or(false));
        input.set_file_workspace(ui.workspace.clone());
        input.set_monochrome(monochrome_requested(ui.cli.no_color));

        input.set_prompt_names(ui.context.prompts.keys().cloned().collect());
        input.set_custom_commands(
            crate::context::commands::load_for_workspace(&ui.workspace)
                .into_iter()
                .map(|(name, template)| (format!("/{name}"), template.description))
                .collect(),
        );
        // Skill discovery walks and hashes every SKILL.md; populate the "/"
        // picker when it lands instead of before the first frame.
        #[cfg(feature = "agent-skills")]
        let skill_commands = ui.instruction_services().ok().map(|services| {
            let workspace = ui.workspace.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                if let Ok(snapshot) = services.refresh(false).await {
                    let rejected = snapshot
                        .diagnostics
                        .iter()
                        .filter(|diagnostic| diagnostic.contains(" rejected: "))
                        .count();
                    let _ = tx.send((skill_command_entries(&snapshot), rejected));
                }
            });
            PendingSkillCommands {
                workspace,
                rx,
                task,
                stale: false,
            }
        });
        #[cfg(not(feature = "agent-skills"))]
        let skill_commands: Option<PendingSkillCommands> = None;
        // The /agent sub-picker and the `@agent:` rows read the same trusted
        // persona source as the task tool's enum.
        #[cfg(feature = "subagents")]
        {
            let (entries, write_names) =
                crate::context::agents::persona_picker_lists(Some(&ui.workspace));
            input.set_agent_names(entries.iter().map(|(name, _)| name.clone()).collect());
            input.set_agent_mentions(entries);
            input.set_agent_write_names(write_names);
        }
        #[cfg(not(feature = "subagents"))]
        input.set_agent_names(ui.context.agent_definitions.keys().cloned().collect());
        input.set_theme_names(ui.context.themes.keys().cloned().collect());
        if let Some(editor) = &ui.cfg.editor {
            input.set_editor(editor.clone());
        }
        input.set_quick_model_names(config::quick_models_map(ui.cfg).keys().cloned().collect());
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

        // No input thread exists yet: retain every unrelated decoded event for
        // its sole owner instead of flushing or racing raw terminal input.
        let background = background_query.collect();
        renderer.set_terminal_background(background.color);
        let theme_warning = crate::context::themes::apply_initial(
            ui.context,
            ui.cfg.colors.as_ref(),
            &mut renderer,
            &mut input,
        );

        let mut run = AgentRunState {
            agent,
            ..AgentRunState::default()
        };
        let chain = ChainState::default();
        let slash = SlashState {
            custom_commands: Default::default(),
            show_reasoning: ui.cfg.resolve_show_reasoning(),
            reasoning_enabled: true,
            todo_tools_enabled: false,
        };
        renderer
            .feed_mut()
            .set_reasoning_display(ui.cfg.resolve_reasoning_display());
        ui.session.reasoning_enabled = slash.reasoning_enabled;
        // The background prebuild measures the real preamble and publishes it
        // when it lands (the statusline shows no overhead until then); only
        // startup paths without a prebuild estimate it here.
        let prebuild_expected = auto_trigger_msg.is_none()
            && run.agent.is_none()
            && !worktree_startup_requested(ui.cli);
        if !prebuild_expected {
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
        }

        render_session(&mut renderer, ui.session, ui.cli, ui.cfg, ui.context)?;
        let marker_path = crate::paths::process_paths_ref()
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
            None,
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
        if let Some(notice) = super::cron::saved_schedule_notice(&ui.session.cron_schedule) {
            renderer.write_line(&sanitize_output(&notice), Color::Yellow)?;
        }
        if let Some(notice) = &ui.cfg.update_notice {
            renderer.write_line(&sanitize_output(notice), Color::DarkGrey)?;
            renderer.write_line("", Color::White)?;
        }
        if let Some(notice) = crate::context::agents::untrusted_personas_notice(&ui.workspace) {
            renderer.write_line(&sanitize_output(&notice), Color::DarkGrey)?;
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
            match task.await {
                Ok(context) => ui.adopt_session_start_context(context),
                Err(error) => {
                    if let Some(owner) = &ui.native_startup {
                        owner.hook_retired(Err(error))?;
                    }
                }
            }
        }

        if auto_trigger_msg.is_some() {
            ui.join_native_startup(&mut run).await?;
        }
        if let Some(ref trigger_msg) = auto_trigger_msg {
            echo_submission(&mut renderer, trigger_msg)?;

            if matches!(&ui.client, crate::provider::AnyClient::Anthropic(_))
                || ui.session.native_history_snapshot().is_some()
            {
                let mut no_prebuild = None;
                crate::startup::heap_future(|| {
                    super::start_main_run(
                        trigger_msg,
                        true,
                        crate::question::QuestionOrigin::NewUserTurn,
                        &mut run,
                        &mut ui,
                        &slash,
                        &mut no_prebuild,
                    )
                })
                .await?;
            } else {
                event_handler::ensure_agent(&mut run.agent, &mut ui, slash.reasoning_enabled).await;
                run.request_tool_results_cleared = ui
                    .session
                    .tool_results_cleared_for_retention(ui.cfg.resolve_keep_recent_tool_results());
                let initial_turn = AutoTriggerTurn::prepare(
                    ui.session,
                    trigger_msg,
                    ui.cfg.resolve_keep_recent_tool_results(),
                );
                #[cfg_attr(not(feature = "hooks"), allow(unused_mut))]
                let (prompt, history, mut pending_turn) = initial_turn.into_runner_inputs();
                run.question_turn = Some(crate::question::QuestionTurn::default());
                ui.session.pending_question = None;
                #[cfg(feature = "agent-skills")]
                {
                    run.instruction_turn =
                        Some(crate::extras::skills::session::InstructionTurn::new(
                            ui.instruction_services()?,
                            None,
                        ));
                }
                let agent = run.agent.as_ref().unwrap().clone();
                let checkpoint_index = pending_turn.message_index();
                let paused = super::build_paused_main_runner(
                    &mut ui,
                    &mut run,
                    agent,
                    prompt,
                    history,
                    checkpoint_index,
                )
                .await;
                #[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
                let mut paused = paused;
                #[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
                super::adopt_runner_hook_context(&mut paused, Some(&mut pending_turn), ui.session);
                let runner = paused.start_interactive();
                super::adopt_started_runner(&mut run, ui.status_signals.as_ref(), runner);
                record_started_main_turn(pending_turn, &mut run, &mut ui);
            }
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
            let native_js_context = ui.session.native_js_context();
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
                        native_js_context,
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

        // SessionStart hooks only add context for the first turn. Keep them
        // (and the native startup join they gate) off the first frame: the
        // run loop adopts them when they finish, and any submission or
        // non-editing event joins them first.
        #[cfg(feature = "hooks")]
        let session_start_task = session_start_task.take();
        #[cfg(not(feature = "hooks"))]
        let session_start_task: Option<SessionStartTask> = None;
        if session_start_task.is_none() {
            ui.join_native_startup(&mut run).await?;
        }

        let (btw_tx, btw_rx) = mpsc::channel::<BtwEvent>(32);
        let (title_tx, title_rx) = mpsc::channel::<SessionTitleResult>(4);
        let (git_status_tx, git_status_rx) = mpsc::channel(1);
        let (command_segment_tx, command_segment_rx) = mpsc::channel(1);

        // Saved `/cron` schedules are held disarmed until `/cron resume`.
        let cron = super::cron::CronStore::from_saved(&ui.session.cron_schedule);
        // The ARCHITECTURE.md offer rides the update-notice slot; it never
        // blocks startup on stdin.
        let update_notice = crate::update::take_startup_notice().or_else(|| {
            #[cfg(feature = "archmd")]
            {
                let offer = interactive_arch_offer_allowed(&ui)
                    .then(|| crate::extras::archmd::startup_offer(ui.workspace.root()))
                    .flatten()?;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = tx.send(offer);
                Some(rx)
            }
            #[cfg(not(feature = "archmd"))]
            None
        });
        let mut app = Self {
            ui,
            run,
            chain,
            slash,
            cron,
            cron_turn: None,
            cron_dynamic: None,
            cron_noop: None,
            renderer,
            input,
            loop_state: EventLoopState::default(),
            rewind_escape: None,
            git_status_tx,
            git_status_rx,
            command_segment_tx,
            command_segment_rx,
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
            deferred_user_events: std::collections::VecDeque::new(),
            #[cfg(feature = "subagents")]
            background_requests: Default::default(),
            running,
            event_handle,
            prebuild,
            terminal_guard,
            #[cfg(feature = "mcp")]
            mcp_admin: None,
            model_cache_generation: crate::ui::slash::model_cache_generation(),
            ctrl_c_exit_armed: false,
            session_start_task,
            skill_commands,
            update_notice,
        };
        app.request_git_status_refresh();
        Ok(app)
    }

    #[cfg_attr(not(feature = "agent-skills"), allow(dead_code))]
    fn mark_startup_skill_commands_stale(&mut self) {
        if let Some(pending) = self.skill_commands.as_mut() {
            pending.stale = true;
        }
    }

    /// Startup work deferred past the first frame is still outstanding.
    fn startup_work_pending(&self) -> bool {
        self.session_start_task.is_some() || self.ui.native_startup.is_some()
    }

    /// Join the deferred SessionStart hook task and native startup
    /// publication. Every turn, command and exit path runs this first, so
    /// they observe exactly the state the former blocking join produced.
    async fn finish_startup_work(&mut self) -> anyhow::Result<()> {
        if let Some(task) = self.session_start_task.take() {
            let result = task.await;
            self.adopt_session_start_result(result)?;
        }
        self.ui.join_native_startup(&mut self.run).await
    }

    fn adopt_session_start_result(
        &mut self,
        result: Result<SessionStartOutput, tokio::task::JoinError>,
    ) -> anyhow::Result<()> {
        match result {
            #[cfg(feature = "hooks")]
            Ok(context) => self.ui.adopt_session_start_context(context),
            #[cfg(not(feature = "hooks"))]
            Ok(never) => match never {},
            #[cfg(feature = "hooks")]
            Err(error) => {
                if let Some(owner) = &self.ui.native_startup {
                    owner.hook_retired(Err(error))?;
                }
            }
            // Without hooks no SessionStart task is ever spawned.
            #[cfg(not(feature = "hooks"))]
            Err(_) => {}
        }
        Ok(())
    }

    /// Start the command-backed statusline segments whose interval elapsed.
    /// Each runs on its own task, off the UI loop, and asks for a repaint only
    /// when its shown text changed.
    fn request_command_segments(&mut self) {
        let specs = crate::ui::statusline::command_specs();
        if specs.is_empty() {
            return;
        }
        let due = crate::ui::statusline_command::due(specs, std::time::Instant::now());
        if due.is_empty() {
            return;
        }
        let input =
            crate::ui::statusline_command::input_json(self.ui.session, self.ui.workspace.root());
        for spec in due {
            let tx = self.command_segment_tx.clone();
            let input = input.clone();
            tokio::spawn(async move {
                if crate::ui::statusline_command::run(spec, input).await {
                    let _ = tx.try_send(());
                }
            });
        }
    }

    /// Dispatch a branch/status refresh when the adaptive schedule says one
    /// is due. Branch detection and the repository fingerprint run on the
    /// blocking pool; `git status` is spawned only when the statusline shows
    /// it and the fingerprint changed (or the observation is stale).
    fn request_git_status_refresh(&mut self) {
        let now = std::time::Instant::now();
        if self.git_status_in_flight || !self.loop_state.git_poll.due(now) {
            return;
        }
        let request = self.loop_state.git_poll.start(now);
        let needs_status = crate::ui::statusline::needs_git_status();
        let working_dir = self.ui.workspace.root().to_path_buf();
        // Branch detection follows the active workspace binding, never the
        // serialized session path.
        let branch_dir = compact_str::CompactString::from(working_dir.to_string_lossy().as_ref());
        let tx = self.git_status_tx.clone();
        self.git_status_in_flight = true;
        tokio::spawn(async move {
            let observe = {
                let working_dir = working_dir.clone();
                let branch_dir = branch_dir.clone();
                move || {
                    (
                        Session::detect_git_branch(&branch_dir),
                        crate::ui::git_poll::fingerprint(&working_dir),
                    )
                }
            };
            let (branch, mut fingerprint) = tokio::task::spawn_blocking(observe)
                .await
                .unwrap_or((None, None));
            let snapshot = if needs_status
                && !request.may_skip_status(&working_dir, fingerprint.as_ref())
            {
                let snapshot = Session::detect_git_status(&working_dir).await;
                // Observe after the status run: it may refresh the index
                // itself, which must not count as the next change.
                let root = working_dir.clone();
                fingerprint =
                    tokio::task::spawn_blocking(move || crate::ui::git_poll::fingerprint(&root))
                        .await
                        .unwrap_or(None);
                Some(snapshot)
            } else {
                None
            };
            let _ = tx
                .send(GitStatusRefresh {
                    working_dir,
                    branch,
                    snapshot,
                    fingerprint,
                })
                .await;
        });
    }

    #[cfg(feature = "hooks")]
    pub(super) async fn finish_session_end(&mut self) -> anyhow::Result<()> {
        // A session switch leaves the previous session's owner behind.
        if let Err(error) = super::rebind_native_session(&mut self.run, &mut self.ui).await {
            tracing::warn!("native session owner could not be rebound at exit: {error:#}");
        }
        if let Some(owner) = self.run.native_session_end.take() {
            owner.run(self.ui.session).await
        } else if !self.ui.cli.no_session
            && crate::session::native_publication::is_native_bearing(self.ui.session)
        {
            // An idle startup has no completed model boundary to publish.
            // Retain its original denied ticket through exact hook cleanup.
            let ticket = self.ui.native_ticket.take().ok_or_else(|| {
                anyhow::anyhow!(
                    "unfinished native context requires recovery before session cleanup"
                )
            })?;
            let scope = crate::agent::runner::AgentWorkScope::new();
            let mut cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
            cleanup.retain_until_settled(ticket);
            scope
                .run(crate::extras::hooks::dispatch_session_end("exit"))
                .await;
            cleanup.settle().await;
            Ok(())
        } else {
            crate::extras::hooks::dispatch_session_end("exit").await;
            Ok(())
        }
    }

    #[cfg(all(feature = "subagents", not(feature = "hooks")))]
    pub(super) async fn finish_session_end(&mut self) -> anyhow::Result<()> {
        // A session switch leaves the previous session's owner behind.
        if let Err(error) = super::rebind_native_session(&mut self.run, &mut self.ui).await {
            tracing::warn!("native session owner could not be rebound at exit: {error:#}");
        }
        if let Some(owner) = self.run.native_session_end.take() {
            owner.run(self.ui.session).await
        } else {
            Ok(())
        }
    }

    #[cfg(feature = "subagents")]
    pub(super) fn persist_background_state(&mut self) -> anyhow::Result<()> {
        if !self.ui.cli.no_session
            && !self.ui.session.background_history.is_empty()
            && !crate::session::native_publication::is_native_bearing(self.ui.session)
        {
            crate::session::storage::save_session_recovering(self.ui.session, |notice| {
                let _ = self
                    .renderer
                    .write_line(&format!("warning: {notice}"), C_ERROR);
            })
        } else {
            Ok(())
        }
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
        crate::session::storage::report_pending_save_notices(self.ui.session, |notice| {
            let _ = self
                .renderer
                .write_line(&format!("warning: {notice}"), C_ERROR);
        });
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
            // Source revalidation follows the existing 1 s tick below; the
            // cheap reply/owner checks still run on every iteration.
            let revalidate = self
                .loop_state
                .last_source_revalidation
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(1));
            if revalidate {
                self.loop_state.last_source_revalidation = Some(std::time::Instant::now());
            }
            self.expire_abandoned_prompts(revalidate)?;
            #[cfg(feature = "subagents")]
            if self.permission_prompt.is_none()
                && self.question_prompt.is_none()
                && let Some(request) = self.background_requests.pop_front()
            {
                self.begin_user_request(request)?;
                self.refresh()?;
            }
            #[cfg(feature = "git-worktree")]
            self.drain_git_observations()?;
            // Every adoption/rebuild path returns here, including a prompt that
            // awaited prebuild and deferred events that immediately continue.
            #[cfg(feature = "mcp")]
            if let Some(manager) = self.ui.mcp_manager.as_mut() {
                let notices = manager.take_notices();
                if !notices.is_empty() {
                    self.renderer.frame(|renderer| {
                        for notice in &notices {
                            renderer.write_line(
                                &sanitize_output(notice.message()),
                                mcp_notice_color(notice.level()),
                            )?;
                        }
                        Ok(())
                    })?;
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
            self.loop_state
                .git_poll
                .observe_running(self.run.is_running, std::time::Instant::now());
            self.request_git_status_refresh();
            self.request_command_segments();
            let tick_deadline = self.loop_state.running_tick.deadline(
                std::time::Instant::now(),
                self.run.is_running,
                self.renderer.chat_needs_redraw(),
            );
            // Only a pending forced refresh (turn end, tool result) wakes an
            // otherwise idle loop; background polls ride on other events.
            let forced_refresh = self
                .loop_state
                .git_poll
                .forced_deadline()
                .filter(|_| !self.git_status_in_flight);
            let cron_wakeup = self.cron_wakeup();
            let cron_repaint = self.cron_label_repaint_at();
            let retry_repaint = self
                .run
                .retry_wait
                .as_ref()
                .filter(|_| self.run.is_running)
                .and_then(|(deadline, _)| {
                    let now = std::time::Instant::now();
                    crate::ui::statusline::retry_wait_repaint_in(*deadline, now)
                        .map(|delay| tokio::time::Instant::now() + delay)
                });
            let job_wake = self.job_wake_armed();
            let job_queue = self.ui.sandbox.job_notice_queue().clone();

            #[cfg(feature = "subagents")]
            let background_pool = self.ui.session.background_pool.clone();
            tokio::select! {
                _ = async {
                    #[cfg(feature = "subagents")]
                    if let Some(pool) = background_pool { pool.wait_changed().await; return; }
                    std::future::pending::<()>().await;
                } => {
                    self.refresh()?;
                    // A finished background task wakes an idle session like a
                    // finished shell job does.
                    #[cfg(feature = "subagents")]
                    crate::startup::heap_future(|| self.wake_for_task_notices()).await?;
                }
                Some(result) = async { Some(self.update_notice.as_mut()?.await) } => {
                    self.update_notice = None;
                    if let Ok(notice) = result {
                        self.renderer.write_line(&sanitize_output(&notice), Color::DarkGrey)?;
                        self.renderer.write_line("", Color::White)?;
                        self.refresh()?;
                    }
                }
                Some(result) = async { Some((&mut self.skill_commands.as_mut()?.rx).await) } => {
                    let pending = self.skill_commands.take().expect("polled pending skills");
                    if let Ok((entries, rejected)) = result
                        && !pending.stale
                        && Arc::ptr_eq(&pending.workspace, &self.ui.workspace)
                    {
                        #[cfg(feature = "agent-skills")]
                        self.input.set_skill_commands(entries);
                        #[cfg(not(feature = "agent-skills"))]
                        let _ = entries;
                        if rejected > 0 {
                            self.renderer.write_line(
                                &format!("{rejected} skill(s) were not loaded; run /skills for details"),
                                C_ERROR,
                            )?;
                            self.refresh()?;
                        }
                    }
                }
                Some(result) = async { Some(self.session_start_task.as_mut()?.await) } => {
                    self.session_start_task = None;
                    self.adopt_session_start_result(result)?;
                    self.ui.join_native_startup(&mut self.run).await?;
                    self.refresh()?;
                }
                Some(()) = self.command_segment_rx.recv() => {
                    self.refresh()?;
                }
                Some(refresh) = self.git_status_rx.recv() => {
                    self.git_status_in_flight = false;
                    let now = std::time::Instant::now();
                    match apply_git_status_refresh(self.ui.session, self.ui.workspace.root(), &refresh) {
                        Some(changed) => {
                            self.loop_state.git_poll.finished(
                                now,
                                refresh.working_dir,
                                refresh.fingerprint,
                                refresh.snapshot.is_some(),
                            );
                            if changed {
                                self.refresh()?;
                            }
                        }
                        None => {
                            self.loop_state.git_poll.retry_now();
                            self.request_git_status_refresh();
                        }
                    }
                }
                Some(ev) = self.user_rx.recv() => {
                    // Construct the batch future in the short factory frame;
                    // it awaits the measured large per-event handler.
                    match crate::startup::heap_future(|| self.handle_user_event_batch(ev)).await? {
                        ControlFlow::Break(()) => break,
                        ControlFlow::Continue(()) => {}
                    }
                }
                Some(prebuilt) = async { self.prebuild.as_mut()?.recv().await }, if !self.run.is_running => {
                    self.take_prebuild(prebuilt).await;
                    self.refresh()?;
                }
                Some(event) = recv_agent_stream_item(&mut self.run.agent_rx), if self.run.agent_rx.is_some() => {
                    match event {
                        AgentStreamItem::Event(event) => {
                            if match &event {
                                AgentEvent::ToolResult { name, .. } => {
                                    crate::agent::runner::tool_may_mutate_workspace(name)
                                }
                                AgentEvent::FileMutation { .. }
                                | AgentEvent::Done { .. }
                                | AgentEvent::Error { .. } => true,
                                _ => false,
                            } {
                                self.loop_state.git_poll.mark_changed(std::time::Instant::now());
                            }
                            // This measured large child is unrelated to input, but
                            // its inline storage inflated every event-loop poll.
                            crate::startup::heap_future(|| self.handle_agent_event(event)).await?;
                            // Streaming paints the chat directly and would leave an
                            // open picker erased until the next key; repaint it.
                            if self.input.picker.as_ref().is_some_and(|picker| picker.active()) {
                                self.refresh()?;
                            }
                        }
                        AgentStreamItem::Closed => {
                            crate::startup::heap_future(|| self.handle_agent_stream_closed()).await?;
                        }
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
                _ = tokio::time::sleep_until(
                    forced_refresh.map_or_else(tokio::time::Instant::now, tokio::time::Instant::from_std)
                ), if forced_refresh.is_some() => {
                    // The next iteration dispatches the due refresh.
                }
                // A `/cron` deadline passed, or a parked fire can start now.
                _ = tokio::time::sleep_until(
                    cron_wakeup.unwrap_or_else(tokio::time::Instant::now)
                ), if cron_wakeup.is_some() => {
                    crate::startup::heap_future(|| self.fire_due_cron()).await?;
                }
                // A background shell job ended while the session is idle.
                _ = job_queue.wait_pending(), if job_wake => {
                    crate::startup::heap_future(|| self.wake_for_job_notices()).await?;
                }
                // The retry-wait countdown stepped to its next second.
                _ = tokio::time::sleep_until(
                    retry_repaint.unwrap_or_else(tokio::time::Instant::now)
                ), if retry_repaint.is_some() => {
                    self.refresh()?;
                }
                // The `/cron` statusline countdown rolled to its next minute.
                _ = tokio::time::sleep_until(
                    cron_repaint.unwrap_or_else(tokio::time::Instant::now)
                ), if cron_repaint.is_some() => {
                    self.refresh()?;
                }
                // A persistent deadline (not a fresh sleep per iteration):
                // it fires even while other events arrive faster than its
                // period, painting the throttled stream tail within one
                // stream interval and keeping the spinner cadence.
                _ = tokio::time::sleep_until(
                    tick_deadline.map_or_else(tokio::time::Instant::now, tokio::time::Instant::from_std)
                ), if tick_deadline.is_some() => {
                    let now = std::time::Instant::now();
                    let spinner_due = self.loop_state.running_tick.fired(now);
                    self.renderer.feed_mut().tick_reasoning_at(now);
                    if self.renderer.paint_pending_chat()?
                        && self.input.picker.as_ref().is_some_and(|picker| picker.active())
                    {
                        self.refresh()?;
                    }
                    if spinner_due {
                        self.renderer.tick_spinner()?;
                    }
                }
                // A tail-first chat paint (a long resumed feed) left earlier
                // blocks unlaid: finish them in small steps between events so
                // scrolling up finds them ready and the history marker gets
                // its exact count.
                _ = tokio::time::sleep(FEED_LAYOUT_STEP_DELAY),
                    if self.renderer.feed_layout_pending() =>
                {
                    self.renderer.advance_feed_layout(FEED_LAYOUT_STEP_BLOCKS)?;
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

    #[cfg(test)]
    pub(crate) async fn teardown(mut self) {
        self.teardown_owners().await;
        self.shutdown_services().await;
    }

    pub(super) async fn teardown_owners(&mut self) {
        // Schedules are session-local; nothing may fire during teardown.
        self.cron.stop_all();
        // Deferred SessionStart/native startup work must finish before session
        // end hooks and the final save observe the session.
        if let Err(error) = self.finish_startup_work().await {
            tracing::warn!(%error, "deferred startup work failed during teardown");
        }
        if let Some(pending) = self.skill_commands.take()
            && tokio::time::timeout(Duration::from_secs(5), pending.task)
                .await
                .is_err()
        {
            tracing::warn!("startup skill discovery did not finish during teardown");
        }
        // The owning actor must retire before its manager/services are closed,
        // including callers that leave without entering the normal run wrapper.
        #[cfg(feature = "mcp")]
        if let Err(error) = self.retire_mcp_work().await {
            tracing::warn!(%error, "TUI MCP operation cleanup failed");
        }
        if let Err(error) = self.retire_session_title().await {
            tracing::warn!(%error,"session title cleanup failed");
        }
        self.cancel_all_permissions();
        self.pause_event_thread();

        // Retire owned work before closing the services it can still use.
        // Side questions, the main run and the prebuild are independent
        // owners: cancel all of them at once and join them concurrently, each
        // within its own bound. The prebuild cancels its scope first and then
        // joins, so an in-flight MCP connect still completes its cleanup.
        const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);
        self.btw_pricing.cancel_all();
        let side_questions =
            futures::future::join_all(self.btw_abort.drain(..).map(|(_, abort, task, scope)| {
                abort.abort();
                retire_scoped_task(task, scope, "side-question", TEARDOWN_TIMEOUT)
            }));
        let prebuild = self.prebuild.take();
        let prebuild = async move {
            match prebuild {
                Some(prebuild) => prebuild.retire(TEARDOWN_TIMEOUT).await,
                None => Ok(()),
            }
        };
        let (side_questions, run, prebuild) =
            tokio::join!(side_questions, self.run.retire(TEARDOWN_TIMEOUT), prebuild);
        for error in side_questions.into_iter().filter_map(Result::err) {
            tracing::warn!(%error, "TUI side-question cleanup failed");
        }
        if let Err(error) = run {
            tracing::warn!(%error, "TUI main-run cleanup failed");
        }
        #[cfg(feature = "git-worktree")]
        if let Err(error) = self.settle_git_usage() {
            tracing::warn!(%error,"Git usage persistence during teardown failed");
        }
        if let Err(error) = prebuild {
            tracing::warn!(%error, "TUI prebuild cleanup failed");
        }

        #[cfg(feature = "subagents")]
        self.ui
            .session
            .close_background_tasks_within(TEARDOWN_TIMEOUT)
            .await;
        // Turn-end snapshots are written off the UI task; every one must be
        // complete before exit returns.
        match tokio::task::spawn_blocking(crate::session::storage::flush_deferred_sessions).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("deferred session save failed: {error:#}"),
            Err(error) => tracing::warn!(%error, "deferred session save join failed"),
        }
    }

    pub(super) async fn shutdown_services(&mut self) {
        #[cfg(feature = "lsp")]
        crate::extras::lsp::shutdown_live_managers().await;
        #[cfg(feature = "mcp")]
        if let Some(mgr) = self.ui.mcp_manager.take() {
            mgr.shutdown().await;
        }
    }

    /// Hand the model picker the current provider's cached ids when a
    /// background listing has landed since the last sync (or when `force`).
    fn sync_live_model_names(&mut self, force: bool) {
        let generation = crate::ui::slash::model_cache_generation();
        // Every frame passes here; allocate the picker's copy only when the
        // session's model actually changed.
        if force || self.loop_state.synced_model.as_ref() != Some(&self.ui.session.model) {
            self.loop_state.synced_model = Some(self.ui.session.model.clone());
            self.input
                .set_current_model(Some(self.ui.session.model.to_string()));
        }
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

    /// A pasted (or dragged-in) path to an image file attaches the image, as
    /// `/add <path>` would, instead of putting the path in the prompt.
    /// Returns whether it did; any doubt (not a single image path, not
    /// readable under the permission policy, text-only model, load failure)
    /// leaves the paste as ordinary text.
    #[cfg(feature = "multimodal")]
    async fn paste_image_path(&mut self, data: &str) -> anyhow::Result<bool> {
        if !self.clipboard_image_allowed() {
            return Ok(false);
        }
        let Some(raw) = super::clipboard_image::pasted_image_path(data) else {
            return Ok(false);
        };
        let expanded = crate::fs::expand_tilde(&raw);
        let path = crate::ui::slash::add::resolve_path(self.ui.workspace.root(), &expanded);
        let Ok(canonical) = path.canonicalize() else {
            return Ok(false);
        };
        if !canonical.is_file() || !self.pasted_path_readable(&canonical) {
            return Ok(false);
        }
        if crate::extras::multimodal::ensure_vision(
            &self.ui.session.provider,
            &self.ui.session.model,
        )
        .is_err()
        {
            return Ok(false);
        }
        let session_id = self.ui.session.id.clone();
        let workspace = self.ui.workspace.clone();
        let loaded = crate::agent::runner::spawn_blocking_scoped(move || {
            let mut attachment = crate::extras::multimodal::load_attachment(&canonical)
                .map_err(|error| error.to_string())?;
            crate::extras::multimodal::fit_image_attachment(&mut attachment).map(|_| attachment)
        })
        .await
        .unwrap_or_else(|_| Err("image load job failed".to_owned()));
        if self.ui.session.id != session_id
            || !Arc::ptr_eq(&workspace, &self.ui.workspace)
            || !self.clipboard_image_allowed()
        {
            return Ok(false);
        }
        match loaded {
            Ok(attachment @ crate::extras::multimodal::MediaAttachment::Image { .. }) => {
                self.ui.session.pending_media.push(attachment);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Whether the permission policy lets the session read `path`; without a
    /// policy, only files inside the workspace qualify.
    #[cfg(feature = "multimodal")]
    fn pasted_path_readable(&self, path: &std::path::Path) -> bool {
        use crate::permission::checker::CheckResult;
        match &self.ui.permission {
            Some(permission) => matches!(
                permission
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .check_path("read", &path.to_string_lossy()),
                CheckResult::Allowed | CheckResult::AllowedWithCoaching(_)
            ),
            None => self
                .ui
                .workspace
                .root()
                .canonicalize()
                .is_ok_and(|root| path.starts_with(root)),
        }
    }

    #[cfg(feature = "multimodal")]
    fn clipboard_image_allowed(&self) -> bool {
        !self.run.is_running
            && self.run.pending_turn.is_none()
            && self.permission_prompt.is_none()
            && self.question_prompt.is_none()
            && self.renderer.chain_prompt.is_none()
            && !self.renderer.chain_but_mode
            && !self.input.picker.as_ref().is_some_and(Picker::active)
    }

    /// Ctrl+V: attach a clipboard image when there is one, else paste the
    /// clipboard's text. With neither, say so; a missing image is not an
    /// error.
    async fn paste_clipboard_key<Fut>(
        &mut self,
        #[cfg(feature = "multimodal")] images: &dyn super::clipboard_image::ClipboardImageReader,
        read_text: impl FnOnce() -> Fut,
    ) -> anyhow::Result<()>
    where
        Fut: std::future::Future<Output = anyhow::Result<String>>,
    {
        #[cfg(feature = "multimodal")]
        if self.clipboard_image_allowed() && self.paste_clipboard_image(images).await? {
            return Ok(());
        }
        match read_text().await {
            Ok(text) if text.is_empty() => {
                self.renderer.show_notice(
                    "nothing to paste: the clipboard has no image or text",
                    C_TOOL,
                );
            }
            Ok(text) => self.input.handle_paste(text),
            Err(error) => self
                .renderer
                .write_line(&format!("paste from clipboard failed: {error}"), C_ERROR)?,
        }
        Ok(())
    }

    #[cfg(feature = "multimodal")]
    async fn paste_clipboard_image(
        &mut self,
        reader: &dyn super::clipboard_image::ClipboardImageReader,
    ) -> anyhow::Result<bool> {
        if !self.clipboard_image_allowed() {
            return Ok(false);
        }
        let session_id = self.ui.session.id.clone();
        let workspace = self.ui.workspace.clone();
        let result = reader.read(&self.ui.sandbox).await;
        if self.ui.session.id != session_id
            || !Arc::ptr_eq(&workspace, &self.ui.workspace)
            || workspace.validate().is_err()
            || !self.clipboard_image_allowed()
        {
            self.renderer.write_line(
                "clipboard image was not attached: session or workspace changed",
                C_ERROR,
            )?;
            return Ok(true);
        }
        // A text-only model cannot use the image: say so instead of staging it.
        if matches!(result, Ok(Some(_)))
            && let Err(refusal) = crate::extras::multimodal::ensure_vision(
                &self.ui.session.provider,
                &self.ui.session.model,
            )
        {
            self.renderer.write_line(
                &format!("clipboard image was not attached: {refusal}"),
                C_ERROR,
            )?;
            return Ok(true);
        }
        match result.and_then(|image| {
            image
                .map(|image| {
                    super::clipboard_image::stage_image(&mut self.ui.session.pending_media, image)
                })
                .transpose()
        }) {
            Ok(Some(_label)) => Ok(true),
            Ok(None) => Ok(false),
            Err(error) => {
                self.renderer.write_line(
                    &format!("clipboard image was not attached: {error}"),
                    C_ERROR,
                )?;
                Ok(true)
            }
        }
    }

    fn refresh(&mut self) -> io::Result<()> {
        if self.loop_state.defer_paint {
            // A coalesced batch of user events paints once at its end.
            self.loop_state.paint_owed = true;
            return Ok(());
        }
        #[cfg(test)]
        {
            self.loop_state.frames += 1;
        }
        self.input.set_file_workspace(self.ui.workspace.clone());
        #[cfg(feature = "subagents")]
        self.input.set_task_ids(
            self.ui
                .session
                .background_tasks()
                .iter()
                .map(|row| crate::extras::subagents::background::short_id(&row.id).to_string())
                .collect(),
        );
        self.sync_live_model_names(false);
        let cron_label = self.cron_status_label();
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
            cron_label.as_deref(),
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
        #[cfg(feature = "subagents")]
        let background = request
            .background
            .as_ref()
            .is_some_and(|origin| origin.valid(self.ui.session));
        #[cfg(not(feature = "subagents"))]
        let background = false;
        if (!self.run.is_running && !background) || request.reply.is_closed() || {
            #[cfg(feature = "subagents")]
            {
                request.background.is_some() && !background
            }
            #[cfg(not(feature = "subagents"))]
            {
                false
            }
        } {
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
        self.expire_abandoned_prompts(true)
    }

    /// Retire prompts whose owner or reply channel is gone. MCP source
    /// revalidation (`revalidate`) is the expensive part: the event loop runs
    /// it only on the 1 s tick, while prompt creation, key handling and the
    /// reply paths always revalidate before an answer can be delivered.
    fn expire_abandoned_prompts(&mut self, revalidate: bool) -> anyhow::Result<()> {
        #[cfg(feature = "mcp")]
        let stale_mcp_question = self
            .question_prompt
            .as_ref()
            .and_then(QuestionState::origin_owner)
            .is_some_and(|owner| !self.mcp_frontend_owner_current_with(owner, revalidate));
        #[cfg(not(feature = "mcp"))]
        let stale_mcp_question = false;
        if stale_mcp_question
            || self
                .question_prompt
                .as_ref()
                .is_some_and(|prompt| prompt.abandoned_with(revalidate))
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
            .is_some_and(|owner| !self.mcp_frontend_owner_current_with(owner, revalidate));
        #[cfg(not(feature = "mcp"))]
        let stale_mcp_permission = false;
        #[cfg(feature = "subagents")]
        let stale_background = self
            .permission_prompt
            .as_ref()
            .is_some_and(|p| p.stale_background(self.ui.session));
        #[cfg(not(feature = "subagents"))]
        let stale_background = false;
        if stale_background
            || stale_mcp_permission
            || self
                .permission_prompt
                .as_ref()
                .is_some_and(|prompt| prompt.abandoned_with(revalidate))
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
        self.cancel_permissions(false);
    }
    fn cancel_all_permissions(&mut self) {
        self.cancel_permissions(true);
    }
    fn cancel_permissions(&mut self, all: bool) {
        if let Some(prompt) = self.question_prompt.take() {
            if let Err(error) = prompt.finish(&mut self.renderer) {
                tracing::warn!(%error,"question overlay cleanup failed");
            }
            drop(prompt);
        }
        #[cfg(feature = "subagents")]
        let keep_background = !all
            && self
                .permission_prompt
                .as_ref()
                .is_some_and(|p| p.background_for(self.ui.session));
        #[cfg(not(feature = "subagents"))]
        let keep_background = false;
        if !keep_background && let Some(prompt) = self.permission_prompt.take() {
            prompt.decline_on_cancel(&self.ui);
            if let Err(error) = prompt.finish(&mut self.renderer) {
                tracing::warn!(%error, "permission overlay cleanup failed");
            }
            drop(prompt);
        }
        if !keep_background {
            self.renderer.permission_prompt = None;
        }
        #[cfg(not(feature = "subagents"))]
        let _ = all;
        if let Some(receiver) = self.ask_rx.as_mut() {
            while let Ok(request) = receiver.try_recv() {
                #[cfg(feature = "subagents")]
                if !all && request.background_for(self.ui.session) {
                    self.background_requests.push_back(request);
                    continue;
                }
                request.cancel();
            }
        }
        #[cfg(feature = "subagents")]
        if all {
            for request in self.background_requests.drain(..) {
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

    /// Whether no modal owns input: no permission/question/chain prompt, no
    /// active picker, Git or plan proposal, MCP operation, or deferred input.
    /// Only in this mode may queued user events be applied without a frame
    /// in between.
    fn plain_input_mode(&self) -> bool {
        let plain = self.permission_prompt.is_none()
            && self.question_prompt.is_none()
            && self.renderer.chain_prompt.is_none()
            && !self.renderer.chain_but_mode
            && !self.input.picker.as_ref().is_some_and(Picker::active)
            && self.chain.plan.pending.is_none()
            && self.deferred_user_events.is_empty();
        #[cfg(feature = "git-worktree")]
        let plain = plain && self.chain.git.pending.is_none();
        #[cfg(feature = "mcp")]
        let plain = plain && self.mcp_admin.is_none();
        plain
    }

    /// Events that only edit the draft or move the view, and so may be
    /// applied back to back with a single frame painted afterwards. Submits,
    /// interrupts, completions, app shortcuts, clicks and every other event
    /// stop a batch and are handled (and painted) individually.
    fn coalescible_user_event(&self, event: &UserEvent) -> bool {
        match event {
            UserEvent::ScrollUp
            | UserEvent::ScrollDown
            | UserEvent::ScrollPageUp
            | UserEvent::ScrollPageDown
            | UserEvent::MouseDrag { .. } => true,
            // An empty paste reads the clipboard image instead.
            UserEvent::Paste(text) => !text.is_empty(),
            UserEvent::Key(key) => {
                key.kind == crossterm::event::KeyEventKind::Press
                    && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                    && matches!(
                        key.code,
                        KeyCode::Char(_)
                            | KeyCode::Backspace
                            | KeyCode::Delete
                            | KeyCode::Left
                            | KeyCode::Right
                            | KeyCode::Home
                            | KeyCode::End
                    )
                    && matches!(
                        self.keyboard_action(*key),
                        None | Some(
                            KeyAction::LineStart
                                | KeyAction::LineEnd
                                | KeyAction::Left
                                | KeyAction::Right
                                | KeyAction::DeleteBackward
                                | KeyAction::DeleteForward
                        )
                    )
            }
            _ => false,
        }
    }

    /// Handle one received user event plus whatever is already queued behind
    /// it, painting once. Wheel notches, drags, typing and pastes arrive far
    /// faster than frames are useful; each is still applied in order with
    /// exactly the sequential semantics, consecutive drags collapse to the
    /// last pointer position, and the batch stops at the first event that is
    /// not a plain edit or that leaves plain input mode (a picker, prompt,
    /// proposal or quit), which is then handled on its own.
    async fn handle_user_event_batch(
        &mut self,
        first: UserEvent,
    ) -> anyhow::Result<ControlFlow<(), ()>> {
        const MAX_BATCH: usize = 64;
        let mut event = first;
        let mut lookahead: Option<UserEvent> = None;
        let mut applied = 0usize;
        loop {
            if !self.coalescible_user_event(&event) || !self.plain_input_mode() {
                self.flush_owed_paint()?;
                if let Some(later) = lookahead.take() {
                    self.deferred_user_events.push_front(later);
                }
                let submits = matches!(&event, UserEvent::Key(key) if key.code == KeyCode::Enter);
                let flow = self.handle_user_event(event).await;
                if submits {
                    // A submitted `!` or `/` command can change the repository.
                    self.loop_state
                        .git_poll
                        .mark_changed(std::time::Instant::now());
                }
                return flow;
            }
            self.loop_state.defer_paint = true;
            let flow = self.handle_user_event(event).await;
            self.loop_state.defer_paint = false;
            let flow = match flow {
                Ok(flow) => flow,
                Err(error) => {
                    self.loop_state.paint_owed = false;
                    return Err(error);
                }
            };
            applied += 1;
            let next = match lookahead.take() {
                Some(next) => Some(next),
                None if flow.is_continue() && applied < MAX_BATCH && self.plain_input_mode() => {
                    self.user_rx.try_recv().ok()
                }
                None => None,
            };
            let Some(mut next) = next else {
                self.flush_owed_paint()?;
                return Ok(flow);
            };
            if flow.is_break() || applied >= MAX_BATCH || !self.plain_input_mode() {
                self.flush_owed_paint()?;
                self.deferred_user_events.push_front(next);
                return Ok(flow);
            }
            if matches!(next, UserEvent::MouseDrag { .. }) {
                // Only the last of consecutive drags decides the selection.
                while let Ok(after) = self.user_rx.try_recv() {
                    if matches!(after, UserEvent::MouseDrag { .. }) {
                        next = after;
                    } else {
                        lookahead = Some(after);
                        break;
                    }
                }
            }
            event = next;
        }
    }

    fn flush_owed_paint(&mut self) -> io::Result<()> {
        if std::mem::take(&mut self.loop_state.paint_owed) {
            self.refresh()?;
        }
        Ok(())
    }

    async fn handle_user_event(&mut self, ev: UserEvent) -> anyhow::Result<ControlFlow<(), ()>> {
        if self.startup_work_pending() && !defers_startup_work(&ev) {
            self.finish_startup_work().await?;
        }
        self.input.set_file_workspace(self.ui.workspace.clone());
        // Without /mcp admin state only a completion or Quit can be handled
        // there; skip boxing that large future for every other event. The
        // boxing itself stays for the startup stack budget.
        #[cfg(feature = "mcp")]
        if (self.mcp_admin.is_some()
            || matches!(ev, UserEvent::McpAdminDone { .. } | UserEvent::Quit))
            && crate::startup::heap_future(|| self.handle_mcp_user_event(&ev)).await?
        {
            return Ok(ControlFlow::Continue(()));
        }
        // handle_plan_event returns false at once unless a plan is pending
        // (its Quit reset also needs one); skip boxing it per keystroke.
        if self.chain.plan.pending.is_some()
            && crate::startup::heap_future(|| self.handle_plan_event(&ev)).await?
        {
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

    /// Modal/operator choices retain their raw default keys. Configuration
    /// cannot synthesize an approval or steal a picker's query/navigation.
    fn keyboard_action(&self, key: KeyEvent) -> Option<KeyAction> {
        if self.permission_prompt.is_some()
            || self.question_prompt.is_some()
            || self.renderer.chain_prompt.is_some()
            || self.renderer.chain_but_mode
            || self.input.picker.as_ref().is_some_and(Picker::active)
        {
            KeyBindings::default().action(key, false)
        } else {
            self.input.keybindings.action(key, false)
        }
    }

    async fn handle_standard_user_event(
        &mut self,
        ev: UserEvent,
    ) -> anyhow::Result<ControlFlow<(), ()>> {
        // A pending approval is never answered by mode cycling. Pickers keep
        // their own BackTab navigation, and modified shortcuts remain ignored.
        if matches!(&ev, UserEvent::Key(key) if self.keyboard_action(*key) == Some(KeyAction::CycleMode))
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
                self.ctrl_c_exit_armed = false;
                if !self.renderer.input_scroll_up() {
                    for _ in 0..self.ui.cfg.resolve_mouse_scroll_lines() {
                        self.renderer.scroll_line_up();
                    }
                }
            }
            UserEvent::ScrollDown => {
                self.ctrl_c_exit_armed = false;
                if self.renderer.is_scrolling() {
                    for _ in 0..self.ui.cfg.resolve_mouse_scroll_lines() {
                        self.renderer.scroll_line_down();
                    }
                } else {
                    self.renderer.input_scroll_down();
                }
            }
            UserEvent::ScrollPageUp => {
                self.ctrl_c_exit_armed = false;
                self.renderer.scroll_page_up();
            }
            UserEvent::ScrollPageDown => {
                self.ctrl_c_exit_armed = false;
                self.renderer.scroll_page_down();
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
                        // A plain click: drop the one-line highlight silently,
                        // and unfold or fold a tool result clicked on its
                        // summary row.
                        let clicked = self.renderer.selection_start.map(|point| point.line);
                        // A link opens on the release of a plain click, so a
                        // press on a URL can still start a drag selection.
                        let url = clicked.and_then(|line| self.renderer.link_url_at(line, col));
                        self.renderer.clear_selection();
                        if let Some(url) = url {
                            self.renderer
                                .show_notice(&format!("opening {url}"), C_AGENT);
                            self.open_link(url);
                        } else if let Some(line) = clicked {
                            self.renderer.toggle_tool_output_at_line(line);
                        }
                    }
                }
            }
            UserEvent::Quit => {
                self.cancel_all_permissions();
                self.cron.stop_all();
                // Background jobs outlive interrupted turns but not the session.
                self.ui.sandbox.cancel_background_jobs();
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
            #[cfg(feature = "export")]
            UserEvent::ShareDone(result) => self.finish_share(result)?,
            UserEvent::LinkOpenFailed(error) => {
                self.renderer
                    .write_line(&format!("cannot open link: {error}"), C_ERROR)?;
            }
            UserEvent::Paste(data) => {
                self.ctrl_c_exit_armed = false;
                #[cfg(feature = "multimodal")]
                if data.is_empty() && self.clipboard_image_allowed() {
                    crate::startup::heap_future(|| {
                        self.paste_clipboard_image(&super::clipboard_image::SystemClipboard)
                    })
                    .await?;
                } else if !self.paste_image_path(&data).await? {
                    self.input.handle_paste(data);
                }
                #[cfg(not(feature = "multimodal"))]
                self.input.handle_paste(data);
            }
            #[cfg(feature = "goal")]
            UserEvent::GoalVerificationDone(event) => {
                self.handle_goal_verification_event(event).await?;
            }
            UserEvent::Key(key) => {
                let action = self.keyboard_action(key);
                match action {
                    Some(KeyAction::ToggleVim) => {
                        self.input.set_vim_enabled(!self.input.vim_enabled());
                        self.refresh()?;
                        return Ok(ControlFlow::Continue(()));
                    }
                    Some(KeyAction::ShowKeys) => {
                        crate::startup::heap_future(|| self.run_slash_command("/keys")).await?;
                        return Ok(ControlFlow::Continue(()));
                    }
                    Some(KeyAction::CopySelection) => {
                        self.ctrl_c_exit_armed = false;
                        self.copy_selection_to_clipboard().await?;
                        self.refresh()?;
                        return Ok(ControlFlow::Continue(()));
                    }
                    Some(KeyAction::PasteClipboard) => {
                        self.ctrl_c_exit_armed = false;
                        crate::startup::heap_future(|| {
                            self.paste_clipboard_key(
                                #[cfg(feature = "multimodal")]
                                &super::clipboard_image::SystemClipboard,
                                read_from_clipboard,
                            )
                        })
                        .await?;
                        self.refresh()?;
                        return Ok(ControlFlow::Continue(()));
                    }
                    _ => {}
                }
                let is_ctrl_c = action == Some(KeyAction::Interrupt);
                let is_ctrl_d = action == Some(KeyAction::Eof);
                if is_ctrl_c {
                    let validation_active = self.run.validation_active();
                    let target =
                        interrupt_target(self.btw_inflight, validation_active, self.run.is_running);
                    let ctrl_c =
                        ctrl_c_action(self.ctrl_c_exit_armed, target, self.input.buffer.is_empty());
                    if ctrl_c == CtrlCAction::ClearDraft {
                        self.input.discard_draft();
                    }
                    if ctrl_c != CtrlCAction::Exit {
                        self.ctrl_c_exit_armed = true;
                    }
                    self.renderer.write_line(ctrl_c_feedback(ctrl_c), C_ERROR)?;
                    // Acknowledgment must reach the terminal before any
                    // potentially slow cancellation and cleanup wait.
                    self.refresh()?;
                    match ctrl_c {
                        CtrlCAction::Interrupt(InterruptTarget::Btw) => {
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
                        CtrlCAction::Interrupt(
                            InterruptTarget::Validation | InterruptTarget::MainRun,
                        ) => {
                            self.abort_main_run().await?;
                        }
                        CtrlCAction::ClearDraft | CtrlCAction::ArmExit => {}
                        CtrlCAction::Exit => return Ok(ControlFlow::Break(())),
                        CtrlCAction::Interrupt(InterruptTarget::Exit) => unreachable!(),
                    }
                    self.refresh()?;
                    return Ok(ControlFlow::Continue(()));
                }
                self.ctrl_c_exit_armed = false;
                if is_ctrl_d {
                    match interrupt_target(
                        self.btw_inflight,
                        self.run.validation_active(),
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
                            match idle_interrupt(true, self.input.buffer.is_empty()) {
                                IdleInterrupt::Exit => return Ok(ControlFlow::Break(())),
                                IdleInterrupt::EditorKey => {
                                    let editor_key = KeyBindings::canonical(KeyAction::Eof)
                                        .expect("default EOF key");
                                    crate::startup::heap_future(|| {
                                        self.handle_key_event(editor_key)
                                    })
                                    .await?;
                                }
                                IdleInterrupt::ClearDraft => unreachable!(),
                            }
                        }
                    }
                    self.refresh()?;
                    return Ok(ControlFlow::Continue(()));
                }

                // The measured key-handler future makes this generic event
                // poll too large for the native 1 MiB stack. Construct it in
                // the existing short factory, retaining the same owned await.
                // `action` was computed for this key above and nothing since
                // changed the modal state it depends on.
                if let Err(e) =
                    crate::startup::heap_future(|| self.handle_key_event_with(key, action)).await
                {
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
                let route = super::clipboard_route::last_route_label();
                self.renderer.show_notice(
                    &if route == "clipboard" {
                        "copied selection".to_owned()
                    } else {
                        format!("copied selection via {route}")
                    },
                    Color::Green,
                );
                self.renderer.clear_selection();
            }
            Ok(ClipboardCopyOutcome::FallbackRequested) => {
                self.renderer.show_notice(
                    &format!(
                        "sent selection to {} (the terminal must allow it)",
                        super::clipboard_route::last_route_label()
                    ),
                    Color::Green,
                );
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
                crate::ui::pickers::sessions::SessionPick::Reload { all, query } => {
                    if self.run.is_running || self.run.pending_turn.is_some() {
                        self.input.picker = None;
                        self.renderer.write_line(
                            "finish or stop the current turn before reopening a session selector",
                            C_ERROR,
                        )?;
                        self.refresh()?;
                        return Ok(true);
                    }
                    self.input
                        .start_loading_session_picker(self.ui.workspace.clone(), all, &query);
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
        let action = self.keyboard_action(key);
        self.handle_key_event_with(key, action).await
    }

    /// [`Self::handle_key_event`] with the key's already-resolved
    /// [`Self::keyboard_action`], so one keystroke resolves bindings once.
    async fn handle_key_event_with(
        &mut self,
        key: KeyEvent,
        action: Option<KeyAction>,
    ) -> anyhow::Result<()> {
        let eligible_escape = key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && key.kind == crossterm::event::KeyEventKind::Press
            && self.input.buffer.is_empty()
            && !self.run.is_running
            && self.permission_prompt.is_none()
            && self.question_prompt.is_none()
            && !self.renderer.selection_active
            && !self.input.picker.as_ref().is_some_and(Picker::active)
            && self.chain.plan.pending.is_none()
            && self.renderer.chain_prompt.is_none();
        if eligible_escape && self.clear_parked_cron_fires()? {
            self.rewind_escape = None;
            return Ok(());
        }
        if eligible_escape {
            let now = std::time::Instant::now();
            if self
                .rewind_escape
                .take()
                .is_some_and(|previous| now.duration_since(previous) <= Duration::from_millis(500))
            {
                self.input.start_rewind_picker(
                    crate::ui::rewind_targets(self.ui.session),
                    RewindBinding::capture(self.ui.session, &self.ui.workspace)?,
                );
                return Ok(());
            } else {
                self.rewind_escape = Some(now);
                // Arming must preserve the ordinary first Escape, including
                // leaving Vim insert mode on an empty draft.
            }
        } else {
            self.rewind_escape = None;
        }
        if self.renderer.selection_active && key.code == KeyCode::Char('y') {
            self.copy_selection_to_clipboard().await?;
            return Ok(());
        }
        if self.renderer.selection_active && key.code == KeyCode::Esc {
            self.renderer.clear_selection();
            return Ok(());
        }
        #[cfg(feature = "export")]
        if key.code == KeyCode::Esc && share_app::cancel_active() {
            self.renderer.write_line("share cancelled", C_AGENT)?;
            return Ok(());
        }

        if action == Some(KeyAction::HistorySearch) {
            if self.input.history_search_active() {
                self.input.history_search_next();
            } else {
                self.input.start_history_search();
            }
            self.refresh()?;
            return Ok(());
        }

        if action == Some(KeyAction::CycleReasoning) {
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

        #[cfg(unix)]
        {
            let modal = self.permission_prompt.is_some()
                || self.question_prompt.is_some()
                || self.renderer.chain_prompt.is_some()
                || self.renderer.chain_but_mode
                || self.input.picker.as_ref().is_some_and(Picker::active);
            let bound = action.is_some() || self.input.keybindings.is_reconfigured(key);
            if suspend_applies(key, bound, modal) {
                return self.suspend_to_shell();
            }
        }

        // Ctrl+T is unbound by default; a user binding for it wins.
        if action.is_none() && is_tool_output_key(key) {
            if self
                .renderer
                .set_tool_output_fold(self.renderer.ctrl_t_fold_target(), None)
                .is_none()
            {
                self.renderer
                    .show_notice("no tool output to expand yet", Color::DarkGrey);
            }
            return Ok(());
        }

        match action {
            Some(KeyAction::PageUp) => {
                self.renderer.scroll_page_up();
                return Ok(());
            }
            Some(KeyAction::PageDown) => {
                self.renderer.scroll_page_down();
                return Ok(());
            }
            _ => {}
        }
        let scroll_key = match action {
            Some(KeyAction::TranscriptTop | KeyAction::TranscriptBottom) => {
                KeyBindings::canonical(action.unwrap())
            }
            _ if !self.input.keybindings.is_reconfigured(key) => Some(key),
            _ => None,
        };
        match scroll_key.and_then(|key| {
            transcript_scroll_key(
                key,
                self.input.buffer.is_empty(),
                self.input.picker.as_ref().is_some_and(|p| p.active()),
            )
        }) {
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

        // Retain the raw picker context even when a caret key closes it
        // before falling through to the editor.
        let picker_owned_key = self.input.picker.as_ref().is_some_and(Picker::active);
        if picker_owned_key && self.input.handle_picker_key(key) {
            if let Some(RewindOutcome::Confirmed(idx, mode, binding)) =
                self.input.take_rewind_outcome()
            {
                if !binding.valid(self.ui.session, &self.ui.workspace) {
                    self.renderer
                        .write_line("rewind selection is stale; open /rewind again", C_ERROR)?;
                    return Ok(());
                }
                #[cfg(feature = "subagents")]
                self.ui.session.reset_background_tasks().await;
                if mode != RewindMode::Conversation {
                    // Like session publication, this bounded native operation stays
                    // owned by the foreground key handler through completion.
                    let restored = self.ui.session.file_checkpoints.restore(
                        &self.ui.session.id,
                        idx,
                        &self.ui.workspace,
                    );
                    match restored {
                        Ok(count) => {
                            if count > 0 {
                                self.ui.session.read_tracker.clear();
                                self.ui.invalidate_prebuild();
                                self.run.agent = None;
                            }
                            self.renderer
                                .write_line(&format!("restored {count} files"), Color::Green)?;
                        }
                        Err(error) => {
                            // A later per-file publication can fail after earlier
                            // paths changed. Never retain read/agent caches on that
                            // partially completed or uncertain restore.
                            self.ui.session.read_tracker.clear();
                            self.ui.invalidate_prebuild();
                            self.run.agent = None;
                            self.renderer.write_line(
                                &format!(
                                    "file rewind refused: {}",
                                    sanitize_output(&error.to_string())
                                ),
                                C_ERROR,
                            )?;
                            return Ok(());
                        }
                    }
                }
                if mode != RewindMode::Files
                    && crate::ui::slash::rewind_selected_turn(self.ui.session, &mut self.input, idx)
                        > 0
                {
                    self.save_session()?;
                    render_session(
                        &mut self.renderer,
                        self.ui.session,
                        self.ui.cli,
                        self.ui.cfg,
                        self.ui.context,
                    )?;
                    self.renderer.write_line(
                        "conversation rewound; /redo restores conversation only",
                        Color::Green,
                    )?;
                } else {
                    self.save_session()?;
                }
            }
            return Ok(());
        }

        if key.code == KeyCode::Char('/') && self.input.cursor == 0 && !self.run.is_running {
            self.refresh_custom_commands();
        }
        #[cfg(any(feature = "mcp", feature = "subagents"))]
        if key.code == KeyCode::Char('@')
            && !self.run.is_running
            && crate::ui::input::mention_can_start_at(&self.input.buffer, self.input.cursor)
        {
            self.refresh_custom_commands();
        }

        if action == Some(KeyAction::OpenEditor) {
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

        if action == Some(KeyAction::Lazygit) {
            self.run_lazygit().await?;
            return Ok(());
        }

        // Chain prompt active: intercept Y/N/B keystrokes
        if self.renderer.chain_prompt.is_some() && !self.renderer.chain_but_mode {
            if self.chain_prompt_premature(key) {
                return Ok(());
            }
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
                self.renderer.chain_prompt = Some(ChainPrompt::new(phase.chain_label()));
                self.chain.label_msg = Some(phase.chain_label().to_string());
            }
            self.input.clear_buffer();
            return Ok(());
        }

        #[cfg(feature = "multimodal")]
        let image_only = self.clipboard_image_allowed()
            && self.input.buffer.trim().is_empty()
            && self.input.keybindings.action(key, true) == Some(KeyAction::Submit)
            && self.ui.session.pending_media.iter().any(|item| {
                matches!(
                    item,
                    crate::extras::multimodal::MediaAttachment::Image { .. }
                )
            });
        #[cfg(not(feature = "multimodal"))]
        let image_only = false;
        let submitted = if image_only {
            self.input.clear_buffer();
            Some(compact_str::CompactString::new(""))
        } else if self.renderer.chain_but_mode || picker_owned_key {
            self.input.handle_operator_draft_key(key)
        } else {
            self.input.handle_key(key)
        };
        if let Some(mut text) = submitted {
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
                    self.renderer.chain_prompt = Some(ChainPrompt::new(phase.chain_label()));
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

            let submission = if image_only {
                super::SubmitAction::Run
            } else {
                classify_submission(self.run.is_running, &text, &self.ui.context.prompts)
            };
            match submission {
                super::SubmitAction::Run => {}
                super::SubmitAction::Ignore => {
                    return Ok(());
                }
                super::SubmitAction::RejectWhileRunning => {
                    // Keep the refused command so it can be sent once the run ends.
                    self.restore_interrupted_prompt(&text);
                    self.renderer.write_line(
                        "agent is running — wait for it to finish or press Ctrl-C before running a command",
                        C_ERROR,
                    )?;
                    return Ok(());
                }
                super::SubmitAction::Queue => {
                    // A run that does not accept steering (none is expected
                    // now that every start records the handle) queues the
                    // message instead of bouncing it back into the editor.
                    if self.ui.cfg.running_input == Some(crate::config::RunningInput::Steer)
                        && self.run.steering.is_some()
                    {
                        self.submit_steering(&text)?;
                        return Ok(());
                    }
                    self.run.pending_inputs.push_back(text.to_string());
                    self.renderer
                        .write_line(&format!("queued: {}", sanitize_output(&text)), C_TOOL)?;
                    return Ok(());
                }
            }

            if let Some(message) = text.trim_start().strip_prefix("/steer")
                && (message.is_empty() || message.starts_with(' '))
            {
                self.submit_steering(message.trim_start())?;
                return Ok(());
            }

            // Bypass-slot handlers: /queue and /btw
            {
                let t = text.trim_start();
                if t == "/queue" || t.starts_with("/queue ") {
                    let arg = t.strip_prefix("/queue").unwrap_or("").trim();
                    echo_submission(&mut self.renderer, text.trim_start())?;
                    self.run_queue_command(arg)?;
                    return Ok(());
                }
            }
            if let Some((expand, arg)) = crate::ui::tool_output::fold_command(&text) {
                echo_submission(&mut self.renderer, &text)?;
                self.run_fold_command(expand, arg);
                return Ok(());
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
                && ((text.starts_with('/') && crate::ui::is_slash_command_text(&text))
                    || crate::ui::slash::is_readonly_diagnostic(&text))
            {
                self.run_slash_command(&text).await?;
            } else if !one_shot && text.starts_with('!') {
                self.run_bang_command(&text).await?;
            } else {
                #[cfg(feature = "mcp")]
                if crate::startup::heap_future(|| self.start_mcp_mentions(&text)).await? {
                    return Ok(());
                }
                #[cfg(feature = "subagents")]
                let Some((text, directive)) = self.start_agent_mentions(&text)? else {
                    return Ok(());
                };
                self.echo_main_submission(&text)?;
                #[cfg(feature = "subagents")]
                self.set_agent_directive(directive);
                self.start_main_run(&text, crate::question::QuestionOrigin::NewUserTurn)
                    .await?;
            }
        }

        Ok(())
    }

    /// Whether a terminal provider error should instead trigger the one
    /// compact-and-retry of this turn (mini-agent-cj9oh): a context-length
    /// rejection, automatic compaction enabled, not already spent this turn,
    /// and an ordinary (non-native, non-worktree-controller) main run.
    fn reactive_context_relief_applies(&self, event: &AgentEvent) -> bool {
        let AgentEvent::Error { message, .. } = event else {
            return false;
        };
        #[cfg(feature = "git-worktree")]
        if self.chain.git.work.is_some() {
            return false;
        }
        self.run.is_running
            && !self.run.reactive_relief_used
            && self.run.native_owner.is_none()
            && self.run.pending_turn.is_some()
            && self.ui.cfg.resolve_compact_enabled()
            && crate::retry::is_context_length_error_message(message)
    }

    async fn handle_agent_event(&mut self, event: AgentEvent) -> anyhow::Result<()> {
        if self.reactive_context_relief_applies(&event)
            && let AgentEvent::Error { interactions, .. } = &event
        {
            self.run.reactive_relief_used = true;
            self.ui.session.adopt_provider_tool_identity(interactions);
            if let Err(error) = crate::ui::reactive_compact_and_respawn(
                interactions,
                &mut self.renderer,
                &mut self.run,
                &mut self.ui,
                &self.slash,
            )
            .await
            {
                return self.fail_context_relief(error).await;
            }
            self.refresh()?;
            return Ok(());
        }
        let terminal_steering = matches!(
            &event,
            AgentEvent::Done { .. } | AgentEvent::Error { .. } | AgentEvent::AwaitingUser { .. }
        )
        .then(|| self.run.steering.clone())
        .flatten();
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
            // Goal/loop rounds are the longest tool-driven turns and
            // `--no-session` compacts the in-memory session, so neither
            // excludes in-turn protection (mini-agent-wr9nw).
            && self.run.is_running
            && (!self.run.mid_turn_compaction_disabled
                || self.ui.session.mid_turn_size_pressure(self.ui.cfg.resolve_compact_size_fraction()))
            && self.ui.cfg.resolve_compact_enabled()
            && self.ui.session.context_window > 0
            && let Some(threshold) = self.ui.cfg.resolve_mid_turn_trigger_threshold(
                self.ui
                    .session
                    .mid_turn_size_pressure(self.ui.cfg.resolve_compact_size_fraction()),
            ) {
            // Image-heavy sessions hit the byte ceiling long before the token
            // threshold: byte pressure makes any boundary a compaction point.
            let real_input_tokens = crate::session::Session::real_input_tokens(
                self.ui.cfg.is_anthropic_native(&self.ui.session.provider),
                usage.input_tokens,
                usage.total_tokens,
                usage.output_tokens,
                usage.cached_input_tokens,
                usage.cache_creation_input_tokens,
            );
            // The completion's own output joins the next request, and a provider
            // that reports no usage still gets protection from the session's
            // estimate (mini-agent-nvnkx.164).
            let real_input_tokens = if real_input_tokens == 0 {
                self.ui.session.effective_context_tokens()
            } else {
                real_input_tokens.saturating_add(usage.output_tokens)
            };
            let pressure = real_input_tokens as f64 / self.ui.session.context_window as f64;
            Some((real_input_tokens, threshold, pressure))
        } else {
            None
        };
        let mid_turn_pressure =
            mid_turn_observation.filter(|(_, threshold, pressure)| pressure > threshold);
        if awaits_compaction_decision
            && let Some(decision_tx) = self.run.compaction_decision_tx.as_ref()
        {
            // Tell the runner when it may skip parking for this boundary: only
            // a completion above the threshold can end in Compact or a stop.
            decision_tx.publish_gate(if mid_turn_observation.is_some() {
                crate::agent::runner::CompactionPressureGate::Limit {
                    window: self.ui.session.context_window,
                    threshold: self
                        .ui
                        .cfg
                        .resolve_mid_turn_trigger_threshold(
                            self.ui.session.mid_turn_size_pressure(
                                self.ui.cfg.resolve_compact_size_fraction(),
                            ),
                        )
                        .unwrap_or(0.0),
                    anthropic_native: self.ui.cfg.is_anthropic_native(&self.ui.session.provider),
                }
            } else {
                crate::agent::runner::CompactionPressureGate::Off
            });
        }

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
                return self.fail_context_relief(error).await;
            }
            self.run.awaiting_compaction_relief = !self.run.mid_turn_compaction_disabled;
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
        // Learn, in event order, whether the runner waited for this boundary,
        // and raise the stop barrier before anything else can let it proceed.
        if awaits_compaction_decision
            && let Some(decision_tx) = self.run.compaction_decision_tx.as_ref()
        {
            decision_tx.take_parked();
            if pressure_action == MidTurnPressureAction::StopContextExhausted {
                decision_tx.request_stop();
            }
        }
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

        if let Some(steering) = terminal_steering {
            self.restore_pending_steering(&steering)?;
        }
        self.finalize_turn().await?;
        if todo_progress_changed {
            self.refresh()?;
        }
        Ok(())
    }

    async fn handle_agent_stream_closed(&mut self) -> anyhow::Result<()> {
        self.run.agent_rx = None;
        if !self.run.is_running {
            self.run.main_abort = None;
            return Ok(());
        }

        tracing::error!(
            response_bytes = self.run.response_buf.len(),
            turn_trace_events = self.run.turn_trace.len(),
            pending_turn = self.run.pending_turn.is_some(),
            main_task_finished = self
                .run
                .main_abort
                .as_ref()
                .is_some_and(tokio::task::AbortHandle::is_finished),
            "agent event stream closed before a terminal completion event"
        );
        self.renderer.write_line(
            "error: agent event delivery ended before terminal completion",
            C_ERROR,
        )?;
        self.fail_pending_main_turn().await;
        self.refresh()?;
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
        // A self-paced cron turn settles before anything below can start the
        // next turn (queued input), so its tool never leaks into that turn.
        if !self.run.is_running {
            self.settle_dynamic_cron_turn().await?;
        }
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
            self.renderer.chain_prompt = Some(ChainPrompt::new(phase.chain_label()));
        }

        if !self.run.is_running {
            self.run.main_abort = None;
            self.cron_turn = None;
            self.cron.end_fire();
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
                #[cfg(feature = "subagents")]
                let Some((next, directive)) = self.start_agent_mentions(&next)? else {
                    return Ok(());
                };
                self.renderer.chain_prompt = None;
                self.renderer.chain_but_mode = false;
                self.chain.pending = None;
                self.chain.label_msg = None;
                echo_submission(&mut self.renderer, &next)?;
                #[cfg(feature = "subagents")]
                self.set_agent_directive(directive);
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
            let persisted = if self.run.native_checkpointed {
                self.run.native_checkpointed = false;
                true
            } else {
                self.save_session_with_status()
            };
            let pending = self
                .run
                .pending_turn
                .take()
                .expect("pending turn checked above");
            if persisted {
                let errors = pending.commit_side_effects(!self.ui.cli.no_session);
                let _ = self.renderer.frame(|renderer| {
                    for error in errors {
                        let _ = renderer.write_line(
                            &format!("warning: failed to append chat history entry: {error}"),
                            C_ERROR,
                        );
                    }
                    Ok(())
                });
            }
        }
    }

    /// Summarizer and admission failures end this turn, while the operator can
    /// still inspect the saved progress and submit another prompt.
    async fn fail_context_relief(&mut self, error: anyhow::Error) -> anyhow::Result<()> {
        let recoverable = error.is::<super::ContextReliefFailure>();
        self.fail_pending_main_turn().await;
        if !recoverable {
            return Err(error);
        }
        self.renderer
            .write_line(&format!("context relief failed: {error:#}"), C_ERROR)?;
        self.refresh()?;
        Ok(())
    }

    /// Exception-safe failure transition with no required presentation. Used
    /// when rendering or mid-turn post-processing itself is what failed.
    async fn fail_pending_main_turn(&mut self) {
        let pending_steering = self.run.steering.clone();
        self.chain.plan = Default::default();
        self.renderer.feed_mut().finish_reasoning();
        self.cancel_pending_permissions();
        let preserve_progress = preserve_pending_main_turn_progress(&mut self.run, self.ui.session);
        if preserve_progress {
            super::record_interrupted_turn_notice(self.ui.session);
        }
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
        if let Err(error) = super::settle_native_main_turn(
            &mut self.run,
            self.ui.session,
            crate::session::native_replay::RetiredNativeOutcome::Incomplete,
        )
        .await
        {
            tracing::warn!(%error, "unfinished native context remains unavailable");
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
        if let Some(steering) = pending_steering
            && let Err(error) = self.restore_pending_steering(&steering)
        {
            tracing::warn!(%error, "pending steering restored despite presentation failure");
        }
    }

    async fn abort_main_run(&mut self) -> anyhow::Result<()> {
        let pending_steering = self.run.steering.clone();
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
        if preserve_progress {
            super::record_interrupted_turn_notice(self.ui.session);
        }
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
        if let Err(error) = super::settle_native_main_turn(
            &mut self.run,
            self.ui.session,
            crate::session::native_replay::RetiredNativeOutcome::Incomplete,
        )
        .await
        {
            tracing::warn!(%error, "unfinished native context remains unavailable");
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
            // A second interrupt with no round settled in between is the user
            // insisting: park the goal rather than leave it armed.
            let insisted = goal.last_evaluation_was_interrupt();
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
            if insisted {
                self.ui.session.goal_store.with_mut(|goal| {
                    goal.set_status(
                        crate::extras::goal::GoalStatus::Paused,
                        Some(crate::extras::goal::PauseReason::UserRequested),
                    )
                });
                let _ = self.renderer.write_line(
                    "goal: paused after a second interrupt; /goal resume to continue",
                    C_AGENT,
                );
            }
            self.chain.loop_label = None;
        }
        // An interrupted scheduler fire keeps its schedule; its payload is not
        // the user's draft. A self-paced fire keeps it only when its turn
        // already chose a wakeup.
        let scheduled = self.cron_turn.take();
        self.cron.end_fire();
        self.settle_dynamic_cron_turn().await?;
        if let Some(text) = failed_prompt
            && scheduled.as_deref() != Some(text.as_str())
        {
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
        if let Some(steering) = pending_steering {
            self.restore_pending_steering(&steering)?;
        }
        self.renderer.write_line(
            "interrupted (changes may be partial; review with git diff)",
            C_ERROR,
        )?;
        let jobs = self.ui.sandbox.running_background_job_count();
        if jobs > 0 {
            self.renderer.write_line(
                &format!(
                    "{jobs} background job{} still running; /jobs lists them and /jobs stop <id> ends one",
                    if jobs == 1 { "" } else { "s" }
                ),
                C_AGENT,
            )?;
        }
        #[cfg(feature = "subagents")]
        if let Some(pool) = &self.ui.session.background_pool
            && let tasks @ 1.. = pool.counts().0
        {
            self.renderer.write_line(
                &format!(
                    "{tasks} background task{} still running; /tasks lists them and /tasks cancel <id> ends one",
                    if tasks == 1 { "" } else { "s" }
                ),
                C_AGENT,
            )?;
        }
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
            PreparedAction::Commit { paths, message } => self.renderer.frame(|renderer| {
                renderer.write_line(
                    "Files requested by write/edit tools this turn (a request may have failed):",
                    C_TOOL,
                )?;
                for path in paths {
                    renderer.write_line(&format!("  {}", sanitize_output(path)), C_TOOL)?;
                }
                renderer.write_line(
                    &format!("Commit message:\n{}", sanitize_output(message)),
                    C_AGENT,
                )
            })?,
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
        self.renderer.chain_prompt = Some(ChainPrompt::new(proposal.action.label()));
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
        let mut placed = false;
        while let Ok(observation) = work.observations.try_recv() {
            placed = true;
            let limit = self.ui.cfg.mutation_diff_options().max_lines;
            let mut count = 0usize;
            let rows = observation
                .text
                .lines()
                .take(limit)
                .inspect(|_| count += 1)
                .map(|line| {
                    let style = if line.starts_with("@@") {
                        crate::ui::feed::BlockStyle::DiffHunk
                    } else if line.starts_with('+') && !line.starts_with("+++") {
                        crate::ui::feed::BlockStyle::DiffAdd
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        crate::ui::feed::BlockStyle::DiffDel
                    } else {
                        crate::ui::feed::BlockStyle::ToolResult
                    };
                    (style, sanitize_output(line))
                });
            self.renderer
                .feed_mut()
                .place_rows_after_anchor(&observation.id, rows);
            if observation.truncated || observation.text.lines().count() > count {
                self.renderer.feed_mut().place_after_anchor(
                    &observation.id,
                    crate::ui::feed::BlockStyle::ToolResult,
                    "[more Git diff rows omitted]",
                );
            }
        }
        // Runs on every event-loop iteration while Git work is pending:
        // repaint only when an observation actually landed in the feed.
        if placed {
            self.renderer.render_viewport()?;
        }
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
            #[cfg(feature = "subagents")]
            let Some((next, directive)) = self.start_agent_mentions(&next)? else {
                return Ok(());
            };
            self.echo_main_submission(&next)?;
            #[cfg(feature = "subagents")]
            self.set_agent_directive(directive);
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
        if self.chain_prompt_premature(key) {
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

    /// True while the chain/plan/git prompt is not armed yet: keys typed into
    /// the draft just before it appeared must not answer it.
    fn chain_prompt_premature(&mut self, key: KeyEvent) -> bool {
        self.chain_prompt_premature_with(key, super::permission_handler::prompt_arming_delay())
    }

    fn chain_prompt_premature_with(&mut self, key: KeyEvent, delay: std::time::Duration) -> bool {
        self.renderer.chain_prompt.as_mut().is_some_and(|p| {
            super::permission_handler::prompt_key_premature(
                &mut p.opened,
                &UserEvent::Key(key),
                std::time::Instant::now(),
                delay,
            )
        })
    }

    fn show_plan_approval(&mut self) -> anyhow::Result<()> {
        self.renderer.chain_but_mode = false;
        self.renderer.chain_prompt = Some(ChainPrompt::new(crate::ui::plan::QUESTION));
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
        if matches!(
            self.keyboard_action(key),
            Some(
                KeyAction::CycleReasoning
                    | KeyAction::PageUp
                    | KeyAction::PageDown
                    | KeyAction::TranscriptTop
                    | KeyAction::TranscriptBottom
            )
        ) || (self.renderer.selection_active
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
        if self.chain_prompt_premature(key) {
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
    ///
    /// Consumes `run.next_turn_directive` on every call, including refused
    /// starts, so a host directive applies to exactly one start attempt.
    async fn start_main_run_prepared(
        &mut self,
        text: &str,
        question_origin: crate::question::QuestionOrigin,
    ) -> anyhow::Result<()> {
        if matches!(
            question_origin,
            crate::question::QuestionOrigin::NewUserTurn
        ) {
            job_wake_app::user_turn_started();
        }
        let turn_directive = self.run.next_turn_directive.take();
        let first_request_tool = self.run.next_turn_first_request_tool.take();
        #[cfg(feature = "multimodal")]
        if let Err(error) =
            super::clipboard_image::user_message(&self.ui.session.pending_media, text)
        {
            self.input.load_text(text);
            self.renderer.write_line(
                &format!(
                    "Image message not started: {}",
                    sanitize_output(&error.to_string())
                ),
                C_ERROR,
            )?;
            return Ok(());
        }
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
        if self.budget_admission_notice_for_user()? {
            self.restore_interrupted_prompt(text);
            return Ok(());
        }
        // Preflight: if the pending payload is irreducibly too large, reject
        // locally before any provider I/O or session mutation.
        let text_tokens = crate::session::Session::estimate_tokens(text);
        // Size-based estimate per attachment (image dimensions, PDF pages);
        // provider usage stays authoritative once it returns.
        #[cfg(feature = "multimodal")]
        let pending_tokens = self
            .ui
            .session
            .pending_media
            .iter()
            .fold(text_tokens, |total, item| {
                total.saturating_add(item.preflight_tokens())
            });
        #[cfg(not(feature = "multimodal"))]
        let pending_tokens = text_tokens;
        let no_quick_models = std::collections::HashMap::new();
        let quick_models = self
            .ui
            .cfg
            .quick_models
            .as_ref()
            .unwrap_or(&no_quick_models);
        let reserve = self.ui.cfg.resolve_reserve_tokens(
            &self.ui.session.model,
            quick_models,
            self.ui.session.context_window,
        );
        if self
            .ui
            .session
            .is_irreducible_with_pending(reserve, pending_tokens)
        {
            // The advice below is to shorten the message: keep it to edit.
            self.restore_interrupted_prompt(text);
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
                     (use /compact or /clear to free space, or shorten the message)"
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
        self.reconcile_native_chain_for_prompt().await?;
        // A refused start is a notice, not an error: an error leaving this
        // function would unwind to the event loop and end the session, and the
        // user's message must stay in the editor.
        #[cfg(feature = "goal")]
        if super::native_goal_blocks_start(&self.ui) {
            self.restore_interrupted_prompt(text);
            self.renderer
                .write_line(super::NATIVE_GOAL_NOTICE, C_ERROR)?;
            return Ok(());
        }
        // An instruction file edited since it was read changes nothing until
        // `/reload`; say so once per change instead of staying silent.
        if matches!(
            question_origin,
            crate::question::QuestionOrigin::NewUserTurn
        ) {
            for notice in self.ui.context.instruction_change_notices() {
                self.renderer
                    .write_line(&sanitize_output(&notice), Color::DarkGrey)?;
            }
        }
        // The echo is already on screen; show the spinner and Working
        // activity before memory refresh, prebuild adoption and agent build.
        self.paint_submission_started()?;
        super::start_main_run_with_directive(
            text,
            turn_directive,
            first_request_tool,
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

    /// Restart an unreplayable native thinking chain before a turn starts and
    /// say so. A restart that cannot be published refuses the start, exactly as
    /// a latched native refusal always did.
    async fn reconcile_native_chain_for_prompt(&mut self) -> anyhow::Result<()> {
        if let Some(notice) = super::reconcile_native_chain(&mut self.run, &mut self.ui).await? {
            self.renderer.write_line(notice, C_RESULT)?;
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
        if self.budget_admission_notice_for_user()? {
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

    /// Automatic starts (queue drain, cron, job wake): the notice is shown once
    /// per session, since nothing the user did triggered the refusal.
    fn budget_admission_notice(&mut self) -> anyhow::Result<bool> {
        let Some(text) = self.budget_refusal_text()? else {
            return Ok(false);
        };
        if self.ui.session.spend_account.take_budget_notice() {
            self.renderer.write_line(&text, C_ERROR)?;
        }
        Ok(true)
    }

    /// User-initiated starts (submit, /review, /init): every refusal says why,
    /// so a retry after the first notice scrolled away is never silent.
    fn budget_admission_notice_for_user(&mut self) -> anyhow::Result<bool> {
        let Some(text) = self.budget_refusal_text()? else {
            return Ok(false);
        };
        self.renderer.write_line(&text, C_ERROR)?;
        Ok(true)
    }

    fn budget_refusal_text(&mut self) -> anyhow::Result<Option<String>> {
        let usd_capped = self.ui.cli.resolve_max_budget_usd(self.ui.cfg)?.is_some();
        if !usd_capped && !self.ui.cfg.has_session_token_ceiling() {
            return Ok(None);
        }
        let run = crate::spend::run::Run::for_session(
            self.ui.session,
            self.ui.cfg,
            crate::spend::RequestKind::Turn,
        );
        if run.admit().is_ok() {
            return Ok(None);
        }
        let reason = run.account.admit_reason(run.prices).err();
        Ok(Some(crate::spend::budget_notice_text(
            reason,
            run.account.snapshot(),
        )))
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
        echo_submission(&mut self.renderer, &msg)?;
        self.run.agent = None;
        self.start_main_run(&msg, question_origin).await?;
        Ok(())
    }

    fn restore_pending_steering(
        &mut self,
        steering: &crate::agent::steering::Steering,
    ) -> anyhow::Result<()> {
        if let Some(text) = steering.take_pending() {
            let retained = if self.input.buffer.is_empty() {
                text
            } else {
                format!("{text}\n{}", self.input.buffer)
            };
            self.input.load_text(&retained);
            self.renderer.write_line(
                "turn ended before another tool boundary; steering restored to draft",
                C_TOOL,
            )?;
        }
        Ok(())
    }

    fn submit_steering(&mut self, text: &str) -> anyhow::Result<()> {
        let result = if self.run.is_running {
            self.run
                .steering
                .as_ref()
                .ok_or("this run does not accept steering")
                .and_then(|steering| steering.submit(text))
        } else {
            Err("/steer requires an active agent turn")
        };
        match result {
            Ok(()) => self.renderer.write_line(
                &format!("steering pending: {}", sanitize_output(text)),
                C_TOOL,
            )?,
            Err(reason) => {
                self.input.load_text(text);
                self.renderer.write_line(reason, C_ERROR)?;
            }
        }
        Ok(())
    }

    /// One frame drawn as running, so a submit shows its spinner while the
    /// turn's startup work (memory refresh, prebuild, agent build) is
    /// awaited. The run state itself is unchanged.
    fn paint_submission_started(&mut self) -> io::Result<()> {
        let was_running = std::mem::replace(&mut self.run.is_running, true);
        let painted = self.refresh();
        self.run.is_running = was_running;
        painted
    }

    fn echo_main_submission(&mut self, text: &str) -> anyhow::Result<()> {
        echo_submission(&mut self.renderer, text)?;
        Ok(())
    }

    /// Resolve `@agent:<name>` mentions in a literal submission.
    ///
    /// Returns the alias-normalised text to echo, persist and send, plus the
    /// non-persisted routing directive for that one turn (`None` without a
    /// mention). An unknown canonical name is refused: the error names the
    /// valid personas, the draft returns to the editor, and `Ok(None)` tells
    /// the caller not to start a turn. Persona definitions from an untrusted
    /// project never resolve.
    #[cfg(feature = "subagents")]
    fn start_agent_mentions(
        &mut self,
        text: &str,
    ) -> anyhow::Result<Option<(String, Option<String>)>> {
        match crate::context::mentions::parse_agent_mentions(text, Some(&self.ui.workspace)) {
            Ok(None) => {
                self.show_bare_persona_hints(text)?;
                Ok(Some((text.to_owned(), None)))
            }
            Ok(Some(mentions)) => {
                self.show_bare_persona_hints(&mentions.text)?;
                let directive = crate::context::mentions::routing_directive(&mentions.names);
                Ok(Some((mentions.text, Some(directive))))
            }
            Err(error) => {
                self.restore_interrupted_prompt(text);
                self.renderer
                    .write_line(&sanitize_output(&error.to_string()), C_ERROR)?;
                self.refresh()?;
                Ok(None)
            }
        }
    }

    /// A bare `@name` that matches a persona is only a file mention; say how
    /// to reach the persona.
    #[cfg(feature = "subagents")]
    fn show_bare_persona_hints(&mut self, text: &str) -> anyhow::Result<()> {
        if !text.contains('@') {
            return Ok(());
        }
        for hint in crate::context::mentions::bare_persona_hints(text, Some(&self.ui.workspace)) {
            self.renderer
                .write_line(&sanitize_output(&hint), crossterm::style::Color::DarkGrey)?;
        }
        Ok(())
    }

    /// Arm the routing directive immediately before the turn start that
    /// consumes it, so a failed echo can never leave it for a later turn.
    #[cfg(feature = "subagents")]
    fn set_agent_directive(&mut self, directive: Option<String>) {
        if directive.is_some() {
            self.run.next_turn_directive = directive;
            self.run.next_turn_first_request_tool = mention_first_request_tool(self.ui.cfg);
        }
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

    /// `/expand [n|all]` and `/collapse [n|all]`: unfold or fold tool
    /// results in place. Feedback is a transient notice so the command never
    /// grows the transcript it is reshaping.
    fn run_fold_command(&mut self, expand: bool, arg: &str) {
        use crate::ui::tool_output::{FoldTarget, parse_fold_target};
        let verb = if expand { "expand" } else { "collapse" };
        match parse_fold_target(arg) {
            Err(error) => self
                .renderer
                .show_notice(&format!("/{verb}: {error}"), C_ERROR),
            Ok(FoldTarget::All) => {
                let changed = self.renderer.set_all_tool_output_folds(expand);
                let done = if expand { "expanded" } else { "collapsed" };
                self.renderer
                    .show_notice(&format!("{done} {changed} tool result(s)"), Color::DarkGrey);
            }
            Ok(FoldTarget::Nth(n)) => {
                if self
                    .renderer
                    .set_tool_output_fold(n, Some(expand))
                    .is_none()
                {
                    let total = self.renderer.feed().fold_count();
                    self.renderer.show_notice(
                        &format!("/{verb}: no tool result #{n} ({total} expandable)"),
                        C_ERROR,
                    );
                }
            }
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
                    let pending = &self.run.pending_inputs;
                    self.renderer.frame(|renderer| {
                        renderer.write_line(&format!("queued ({}):", pending.len()), C_TOOL)?;
                        for (i, q) in pending.iter().enumerate() {
                            renderer.write_line(
                                &format!("  {}. {}", i + 1, sanitize_output(q)),
                                C_TOOL,
                            )?;
                        }
                        Ok(())
                    })?;
                }
            }
            _ => self
                .renderer
                .write_line("usage: /queue [ls|clear|pop]", C_ERROR)?,
        }
        Ok(())
    }

    async fn run_btw(&mut self, text: &str) -> anyhow::Result<()> {
        echo_submission(&mut self.renderer, text)?;
        let btw_text = text
            .trim_start()
            .strip_prefix("/btw")
            .map(|s| s.trim())
            .unwrap_or("");
        if btw_text.is_empty() {
            self.renderer.write_line("usage: /btw <message>", C_ERROR)?;
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

    async fn handle_reasoning_control(&mut self, text: &str) -> anyhow::Result<bool> {
        use crate::config::types::ReasoningEffort as E;
        use crate::provider::effort::ThinkingMode;
        let parts = text.split_whitespace().collect::<Vec<_>>();
        let Some(command) = parts.first().copied() else {
            return Ok(false);
        };
        let native_mode = matches!(command, "/reasoning" | "/thinking")
            && self
                .ui
                .client
                .has_native_reasoning_controls(&self.ui.session.model);
        if command != "/effort" && !native_mode {
            return Ok(false);
        }
        echo_submission(&mut self.renderer, text)?;
        let current = self.ui.session.reasoning_controls;
        if command == "/effort" && parts.len() == 1 {
            match self
                .ui
                .client
                .resolve_session_reasoning(self.ui.cfg, self.ui.session, current)
            {
                Ok((effective, _)) => self.renderer.write_line(
                    &format!(
                        "effort: {} ({}){}",
                        effective.effort.map_or("provider default", E::as_wire_str),
                        effective.source.label(),
                        if effective.adaptive {
                            "; adaptive thinking"
                        } else {
                            ""
                        }
                    ),
                    C_RESULT,
                )?,
                Err(error) => self.renderer.write_line(&error.to_string(), C_ERROR)?,
            }
            return Ok(true);
        }
        let mut next = current;
        if command == "/effort" {
            next.effort = match parts.as_slice() {
                [_, "default"] => None,
                [_, "low"] => Some(E::Low),
                [_, "medium"] => Some(E::Medium),
                [_, "high"] => Some(E::High),
                [_, "xhigh"] => Some(E::Xhigh),
                [_, "max"] => Some(E::Max),
                _ => {
                    self.renderer.write_line(
                        "usage: /effort [low|medium|high|xhigh|max|default]",
                        C_ERROR,
                    )?;
                    return Ok(true);
                }
            };
        } else {
            next.thinking = match parts.as_slice() {
                [_] => {
                    let enabled = self
                        .ui
                        .client
                        .resolve_session_reasoning(self.ui.cfg, self.ui.session, current)
                        .ok()
                        .and_then(|(_, enabled)| enabled);
                    let Some(enabled) = enabled else {
                        self.renderer.write_line("this model has no verified thinking mode; use /reasoning on or off with a supported model",C_ERROR)?;
                        return Ok(true);
                    };
                    Some(if enabled {
                        ThinkingMode::Off
                    } else {
                        ThinkingMode::On
                    })
                }
                [_, "on"] => Some(ThinkingMode::On),
                [_, "off"] => Some(ThinkingMode::Off),
                [_, "default"] => None,
                _ => {
                    self.renderer
                        .write_line("usage: /reasoning [on|off|default]", C_ERROR)?;
                    return Ok(true);
                }
            };
        }
        if self.run.is_running
            || self.run.native_owner.is_some()
            || self.run.pending_turn.is_some()
            || self.permission_prompt.is_some()
            || self.question_prompt.is_some()
            || self.ui.session.pending_question.is_some()
        {
            self.renderer.write_line(
                "finish or stop the current turn before changing effort or thinking",
                C_ERROR,
            )?;
            return Ok(true);
        }
        let effective =
            match self
                .ui
                .client
                .resolve_session_reasoning(self.ui.cfg, self.ui.session, next)
            {
                Ok(effective) => effective,
                Err(error) => {
                    self.renderer.write_line(&error.to_string(), C_ERROR)?;
                    return Ok(true);
                }
            };
        if next == current {
            self.renderer
                .write_line("reasoning controls already match this setting", C_RESULT)?;
            return Ok(true);
        }
        let native = crate::session::native_publication::is_native_bearing(self.ui.session)
            || matches!(&self.ui.client, crate::provider::AnyClient::Anthropic(_));
        if native && !self.ui.cli.no_session {
            let result = if let Some(owner) = &mut self.run.native_session_end {
                crate::startup::heap_future(|| owner.update_reasoning(self.ui.session, next)).await
            } else if let Some(ticket) = &mut self.ui.native_ticket {
                let scope = crate::agent::runner::AgentWorkScope::new();
                let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
                let result = crate::session::native_publication::startup_update_reasoning(
                    self.ui.session,
                    ticket,
                    next,
                    &scope,
                )
                .await;
                cleanup.settle().await;
                result
            } else {
                Err(anyhow::anyhow!(
                    "unfinished native context requires recovery before changing reasoning controls"
                ))
            };
            if let Err(error) = result {
                self.renderer.write_line(&error.to_string(), C_ERROR)?;
                return Ok(true);
            }
        } else {
            self.ui.session.reasoning_controls = next;
            if !self.save_session_with_status() {
                self.ui.session.reasoning_controls = current;
                return Ok(true);
            }
        }
        if let Some(agent) = self.run.agent.take() {
            self.run.agent = Some(
                crate::startup::heap_future(|| {
                    std::future::ready(agent.with_reasoning_controls(self.ui.cfg, next))
                })
                .await,
            );
        }
        self.renderer.write_line(
            &format!(
                "effort: {} ({}){}",
                effective
                    .0
                    .effort
                    .map_or("provider default", E::as_wire_str),
                effective.0.source.label(),
                if effective.0.adaptive {
                    "; adaptive thinking"
                } else {
                    ""
                }
            ),
            C_RESULT,
        )?;
        if native_mode {
            self.renderer.write_line(
                match next.thinking {
                    Some(ThinkingMode::On) => "thinking: on",
                    Some(ThinkingMode::Off) => "thinking: off",
                    None => "thinking: provider default",
                },
                C_RESULT,
            )?;
        }
        // Display collapse/off preferences and the original native system
        // preamble remain independent from these request controls.
        Ok(true)
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
        if crate::startup::heap_future(|| self.handle_reasoning_control(text)).await? {
            return Ok(());
        }
        if self.run.is_running && !super::allowed_while_running(text) {
            let message = if text.split_whitespace().next() == Some("/goal") {
                "a goal round is running: press Esc or run /goal pause first \
                 (/goal status, bounds and check work while it runs)"
            } else {
                "finish or stop the current turn before changing model, session or context"
            };
            self.renderer.write_line(message, C_ERROR)?;
            return Ok(());
        }
        if (selected_session.is_some() || text.split_whitespace().next() == Some("/fork"))
            && (self.run.is_running
                || self.permission_prompt.is_some()
                || self.question_prompt.is_some()
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
                Some("/import" | "/fork" | "/new" | "/clear" | "/rename" | "/worktree")
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

        if crate::ui::slash::config::is_command(text)
            && !crate::ui::slash::config::is_browse(text)
            && (self.permission_prompt.is_some()
                || self.run.pending_turn.is_some()
                || self.ui.session.pending_question.is_some())
        {
            self.renderer.write_line(
                "Finish pending approval or clarification before saving settings",
                C_ERROR,
            )?;
            return Ok(());
        }
        if crate::ui::slash::config::is_command(text) {
            self.renderer.write_line("", Color::White)?;
        } else {
            echo_submission(&mut self.renderer, text)?;
        }
        #[cfg(feature = "export")]
        if share_app::is_confirm(text) {
            return self.start_share();
        }
        #[cfg(feature = "export")]
        if share_app::is_unshare(text) {
            return self.start_unshare();
        }

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
        if let Some(decision_tx) = self.run.compaction_decision_tx.as_ref() {
            // A command may change the model or window the runner's pressure
            // gate was published for.
            decision_tx.invalidate_gate();
        }
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
                &mut self.cron,
            )
        })
        .await;
        if needs_tty {
            self.rebind_event_thread();
        }
        if prior_question_session != self.ui.session.id {
            // /new and /resume start a fresh wake budget.
            job_wake_app::user_turn_started();
            self.adopt_saved_cron()?;
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

        // /model, /compact and friends change what the saved native thinking
        // chain was built for. Restart it now, while the session is idle, so the
        // next prompt starts a clean epoch instead of failing to replay.
        if prior_question_session == self.ui.session.id {
            match super::reconcile_native_chain(&mut self.run, &mut self.ui).await {
                Ok(Some(notice)) => self.renderer.write_line(notice, C_RESULT)?,
                Ok(None) => {}
                Err(error) => self.renderer.write_line(
                    &format!(
                        "Could not restart the native thinking context: {}",
                        sanitize_output(&error.to_string())
                    ),
                    C_ERROR,
                )?,
            }
        }

        if crate::ui::slash::config::is_command(text)
            || crate::ui::slash::is_readonly_view(text)
            || crate::ui::slash::is_jobs_control(text)
            || crate::ui::slash::is_tasks_control(text)
            || crate::ui::slash::is_input_control(text)
        {
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

        // Branching already persisted its new identity before activation.
        // Neither valid nor invalid forks may start a copied first goal round
        // or rewrite the original through generic command settlement.
        if name == Some("/fork") {
            if result
                .as_ref()
                .is_err_and(crate::ui::slash::is_persistence_restart_required)
            {
                return result;
            }
            if let Err(error) = result {
                self.renderer
                    .write_line(&sanitize_output(&error.to_string()), C_ERROR)?;
            }
            return Ok(());
        }

        // The provider may have changed: warm its model list in the
        // background (never blocking the command) and show what is cached.
        let context_diagnostic = crate::ui::slash::is_readonly_diagnostic(text);
        // Successful diagnostics cannot enter the generic success settlement:
        // that saves history and may auto-start a pending first goal round.
        if context_diagnostic && result.is_ok() {
            return Ok(());
        }
        if !is_quit_command(text) && !context_diagnostic && name != Some("/fork") {
            let provider = self.ui.session.provider.to_string();
            let is_custom = self.ui.cfg.custom_providers_map().contains_key(&provider);
            crate::ui::slash::warm_model_cache(&provider, is_custom, &self.ui.client);
            // Re-clone the picker's ids only when the listing route changed;
            // a finished background listing bumps the generation instead.
            let route = (is_custom, self.ui.client.listing_key(&provider).ok());
            let route_changed = self.loop_state.synced_route.as_ref() != Some(&route);
            if route_changed {
                self.loop_state.synced_route = Some(route);
            }
            self.sync_live_model_names(route_changed);
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
            echo_submission(&mut self.renderer, text)?;
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
                .filter(|(name, package)| {
                    package.record.user_invocable
                        && !crate::ui::pickers::list::reserved_command(name)
                })
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
            self.mark_startup_skill_commands_stale();
            self.input.set_skill_commands(entries.clone());
            self.renderer.frame(|renderer| {
                renderer.write_line(
                    "Agent Skills (instruction context; no permission grants):",
                    C_TOOL,
                )?;
                for (name, description) in &entries {
                    let package = snapshot.packages.get(name.trim_start_matches('/'));
                    let hint = package
                        .and_then(|package| package.record.argument_hint.as_deref())
                        .map(|hint| format!(" {hint}"))
                        .unwrap_or_default();
                    renderer.write_line(
                        &format!(
                            "  {}{}  {}",
                            sanitize_output(name),
                            sanitize_output(&hint),
                            description
                        ),
                        C_TOOL,
                    )?;
                    for diagnostic in package.iter().flat_map(|p| p.record.diagnostics.iter()) {
                        renderer.write_line(
                            &format!("    note: {}", sanitize_output(diagnostic)),
                            C_ERROR,
                        )?;
                    }
                }
                if entries.is_empty() {
                    renderer.write_line("  no currently authorized skills", C_TOOL)?;
                }
                #[cfg(feature = "agent-skills")]
                for note in crate::extras::skills::session::extra_root_notes() {
                    renderer.write_line(&sanitize_output(&note), C_ERROR)?;
                }
                for diagnostic in snapshot.diagnostics.iter().take(16) {
                    renderer.write_line(&sanitize_output(diagnostic), C_ERROR)?;
                }
                Ok(())
            })?;
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
            // A package that was found but rejected is not an unknown command:
            // say why it is not available.
            if let Some(reason) = rejected_skill_reason(&snapshot.diagnostics, name) {
                self.renderer
                    .write_line(&sanitize_output(&reason), C_ERROR)?;
                return Ok(true);
            }
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
        let mut pin = match services.pin(name).await {
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
        pin.arguments = arguments.to_string();
        // `context: fork`: the turn's first request must delegate the skill
        // body to a `task` child (a no-op where `task` is not available).
        #[cfg(feature = "subagents")]
        if snapshot
            .packages
            .get(name)
            .is_some_and(|package| package.record.context_fork)
        {
            self.run.next_turn_first_request_tool = Some(crate::agent::first_tool::TASK_TOOL_NAME);
        }
        crate::ui::ensure_agent(
            &mut self.run.agent,
            &mut self.ui,
            self.slash.reasoning_enabled,
        )
        .await;
        // A forked skill delegates through `task`; without it the skill
        // renders inline with a notice instead of naming a missing tool.
        pin.task_available = match self.run.agent.as_ref() {
            Some(agent) => agent.advertises_tool("task").await,
            None => false,
        };
        let turn =
            crate::extras::skills::session::InstructionTurn::new(services, Some(pin.clone()));
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
        let commands = crate::context::commands::load_cached(
            &mut self.slash.custom_commands,
            &self.ui.workspace,
        )
        .into_iter()
        .flatten()
        .map(|(name, template)| (format!("/{name}"), template.description.clone()))
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
        #[cfg(feature = "subagents")]
        {
            let (entries, write_names) =
                crate::context::agents::persona_picker_lists(Some(&self.ui.workspace));
            self.input
                .set_agent_names(entries.iter().map(|(name, _)| name.clone()).collect());
            self.input.set_agent_mentions(entries);
            self.input.set_agent_write_names(write_names);
        }
        self.input.set_custom_commands(commands);
    }

    /// Runs a template's inline ``!`command` `` spans through the sandboxed
    /// user-shell path. The command file grants nothing: every span must be
    /// allowed by the existing shell permission rules (a deny rule refuses the
    /// expansion; a span that would need an approval prompt refuses it too,
    /// naming every such command), so nothing runs unless every span may.
    async fn run_template_shell(&mut self, commands: &[String]) -> Result<Vec<String>, String> {
        use crate::permission::checker::CheckResult;
        let mut needs_approval = Vec::new();
        if let Some(permission) = &self.ui.permission {
            let mut checker = permission.lock().unwrap_or_else(|e| e.into_inner());
            for command in commands {
                match checker.check("shell", command) {
                    CheckResult::Allowed | CheckResult::AllowedWithCoaching(_) => {}
                    CheckResult::Ask => needs_approval.push(command.clone()),
                    CheckResult::Denied(reason) => {
                        return Err(format!("inline command !`{command}` is denied: {reason}"));
                    }
                }
            }
        }
        if !needs_approval.is_empty() {
            return Err(format!(
                "inline shell commands need an allow rule for the shell tool (nothing was run): {}",
                needs_approval
                    .iter()
                    .map(|command| format!("`{command}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let sandbox = self.ui.sandbox.clone();
        let mut outputs = Vec::with_capacity(commands.len());
        for command in commands {
            let run = sandbox
                .run_explicit_shell(
                    &format!("!{command}"),
                    crate::sandbox::configured_command_limits(),
                    None,
                )
                .await
                .map_err(|error| format!("inline command !`{command}` failed to start: {error}"))?;
            outputs.push(crate::context::commands::format_shell_output(
                command,
                &run.rendered_output(),
                run.succeeded(),
            ));
        }
        Ok(outputs)
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
        let shell_commands = template.shell_commands();
        let shell_outputs = if shell_commands.is_empty() {
            None
        } else {
            match self.run_template_shell(&shell_commands).await {
                Ok(outputs) => Some(outputs),
                Err(error) => {
                    self.renderer.write_line(
                        &sanitize_output(&format!("custom command: {error}")),
                        C_ERROR,
                    )?;
                    return Ok(());
                }
            }
        };
        let message = match template.expand_full(
            &command.arguments,
            &self.ui.workspace,
            &self.ui.permission,
            shell_outputs.as_deref(),
        ) {
            Ok((message, notices)) => {
                for notice in notices {
                    self.renderer.write_line(
                        &sanitize_output(&format!("custom command: {notice}")),
                        Color::DarkGrey,
                    )?;
                }
                message
            }
            Err(error) => {
                self.renderer
                    .write_line(&format!("custom command: {error}"), C_ERROR)?;
                return Ok(());
            }
        };
        let reserve = self.ui.cfg.resolve_reserve_tokens(
            &self.ui.session.model,
            config::quick_models_map(self.ui.cfg),
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
            if let Some(decision_tx) = self.run.compaction_decision_tx.as_ref() {
                decision_tx.invalidate_gate();
            }
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
                    &mut self.cron,
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
            Err(e)
                if matches!(
                    e.downcast_ref::<DeferredSlash>(),
                    Some(DeferredSlash::Compress(_))
                ) =>
            {
                let Ok(DeferredSlash::Compress(instructions)) = e.downcast::<DeferredSlash>()
                else {
                    unreachable!("guard matched Compress")
                };
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
                if matches!(
                    e.downcast_ref::<DeferredSlash>(),
                    Some(DeferredSlash::McpLogin(_))
                ) =>
            {
                let Ok(DeferredSlash::McpLogin(server)) = e.downcast::<DeferredSlash>() else {
                    unreachable!("guard matched McpLogin")
                };
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
                        crate::startup::heap_future(|| self.refresh_worktree_workspace_context())
                            .await?;
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
                        crate::startup::heap_future(|| {
                            self.handle_worktree_merge(info.clone(), target.clone(), true)
                        })
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
            Err(e)
                if matches!(
                    e.downcast_ref::<DeferredSlash>(),
                    Some(DeferredSlash::Init(_))
                ) =>
            {
                let Ok(DeferredSlash::Init(prompt)) = e.downcast::<DeferredSlash>() else {
                    unreachable!("guard matched Init")
                };
                if matches!(&self.ui.client, crate::provider::AnyClient::Anthropic(_))
                    || self.ui.session.native_history_snapshot().is_some()
                {
                    crate::startup::heap_future(|| self.start_internal_main_run(&prompt)).await?;
                } else {
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
                    let checkpoint_index = self.ui.session.messages.len();
                    let paused = super::build_paused_main_runner(
                        &mut self.ui,
                        &mut self.run,
                        agent,
                        prompt,
                        history,
                        checkpoint_index,
                    )
                    .await;
                    #[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
                    let mut paused = paused;
                    #[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
                    super::adopt_runner_hook_context(
                        &mut paused,
                        self.run.pending_turn.as_mut(),
                        self.ui.session,
                    );
                    let runner = paused.start_interactive();
                    super::adopt_started_runner(
                        &mut self.run,
                        self.ui.status_signals.as_ref(),
                        runner,
                    );
                }
            }
            Err(e)
                if matches!(
                    e.downcast_ref::<DeferredSlash>(),
                    Some(DeferredSlash::Review(_))
                ) =>
            {
                let Ok(DeferredSlash::Review(msg)) = e.downcast::<DeferredSlash>() else {
                    unreachable!("guard matched Review")
                };
                self.chain.dot_prompt_restore = self.ui.context.one_shot_restore.take();
                self.start_internal_main_run(&msg).await?;
            }
            #[cfg(feature = "memory")]
            Err(e)
                if matches!(
                    e.downcast_ref::<DeferredSlash>(),
                    Some(DeferredSlash::Editor(_))
                ) =>
            {
                let Ok(DeferredSlash::Editor(path)) = e.downcast::<DeferredSlash>() else {
                    unreachable!("guard matched Editor")
                };
                let path = path.display().to_string();
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
                    && let Some(goal) =
                        self.ui.session.goal_store.snapshot().filter(|g| {
                            g.autostart && g.status.is_running() && g.progress.rounds == 0
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
                    let checkpoint_index = self.ui.session.messages.len();
                    let paused = super::build_paused_main_runner(
                        &mut self.ui,
                        &mut self.run,
                        agent,
                        prompt,
                        std::sync::Arc::from(Vec::new()),
                        checkpoint_index,
                    )
                    .await;
                    #[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
                    let mut paused = paused;
                    #[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
                    super::adopt_runner_hook_context(
                        &mut paused,
                        self.run.pending_turn.as_mut(),
                        self.ui.session,
                    );
                    let runner = paused.start_interactive();
                    super::adopt_started_runner(
                        &mut self.run,
                        self.ui.status_signals.as_ref(),
                        runner,
                    );
                    self.run.goal_round = Some(crate::extras::goal::driver::RoundCollector::new());
                }
            }
        }
        Ok(())
    }

    /// One user event while a `!` command runs. Resizes, scrolling and plain
    /// editing keys act at once; the interrupt key (as bound, plus Ctrl+C)
    /// cancels the command. Anything else is returned so the caller can park
    /// it until the command ends.
    fn bang_live_event(
        &mut self,
        event: UserEvent,
        cancellation: &CommandCancellation,
    ) -> anyhow::Result<Option<UserEvent>> {
        match &event {
            UserEvent::Resize => {
                self.renderer.resize();
                self.renderer.invalidate();
                self.refresh()?;
                return Ok(None);
            }
            UserEvent::ScrollPageUp => {
                self.renderer.scroll_page_up();
                return Ok(None);
            }
            UserEvent::ScrollPageDown => {
                self.renderer.scroll_page_down();
                return Ok(None);
            }
            UserEvent::ScrollUp => {
                for _ in 0..self.ui.cfg.resolve_mouse_scroll_lines() {
                    self.renderer.scroll_line_up();
                }
                return Ok(None);
            }
            UserEvent::ScrollDown => {
                for _ in 0..self.ui.cfg.resolve_mouse_scroll_lines() {
                    self.renderer.scroll_line_down();
                }
                return Ok(None);
            }
            UserEvent::Key(key) => {
                let key = *key;
                if key.kind == crossterm::event::KeyEventKind::Release {
                    return Ok(None);
                }
                let action = self.keyboard_action(key);
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                if matches!(action, Some(KeyAction::Interrupt | KeyAction::Eof)) || ctrl_c {
                    cancellation.cancel();
                    self.renderer.show_notice("cancelling…", Color::Yellow);
                    self.refresh()?;
                    return Ok(None);
                }
                match action {
                    Some(KeyAction::PageUp) => {
                        self.renderer.scroll_page_up();
                        return Ok(None);
                    }
                    Some(KeyAction::PageDown) => {
                        self.renderer.scroll_page_down();
                        return Ok(None);
                    }
                    Some(KeyAction::TranscriptTop) => {
                        self.renderer.scroll_to_top();
                        return Ok(None);
                    }
                    Some(KeyAction::TranscriptBottom) => {
                        self.renderer.scroll_to_bottom()?;
                        return Ok(None);
                    }
                    _ => {}
                }
                // Typing and cursor movement go to the draft; Enter and
                // chords that could submit or open pickers wait.
                let plain = !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                let editing = plain
                    && matches!(
                        key.code,
                        KeyCode::Char(_)
                            | KeyCode::Backspace
                            | KeyCode::Delete
                            | KeyCode::Left
                            | KeyCode::Right
                    );
                if editing && self.input.picker.is_none() && !self.input.vim_enabled() {
                    if let Some(submitted) = self.input.handle_key(key) {
                        // Never submit while the command runs.
                        self.input.load_text(&submitted);
                    }
                    self.refresh()?;
                    return Ok(None);
                }
            }
            _ => {}
        }
        Ok(Some(event))
    }

    async fn run_bang_command(&mut self, text: &str) -> anyhow::Result<()> {
        /// Live output of a `!` command is repainted at most this often.
        const BANG_PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
        /// Trailing output lines shown while a `!` command runs.
        const BANG_PROGRESS_LINES: usize = 12;
        let command_is_empty = text
            .strip_prefix('!')
            .is_none_or(|command| command.trim().is_empty());
        if command_is_empty {
            self.renderer
                .write_line("error: empty command after '!'", C_ERROR)?;
            return Ok(());
        }
        // The blank separator anchors the live output tail shown while the
        // command runs; the final output replaces it after the save. The
        // echo and the anchor paint as one frame before the command starts.
        let progress_anchor = format!("bang-shell:{}", uuid::Uuid::new_v4());
        self.renderer.frame(|renderer| {
            for line in text.lines() {
                renderer.write_line(&format!("> {}", sanitize_output(line)), Color::Green)?;
            }
            renderer.write_line_anchored(&progress_anchor, "", Color::White)
        })?;

        let cancellation = CommandCancellation::new();
        let interrupt_key = self
            .input
            .keybindings
            .action_keys()
            .into_iter()
            .find(|(name, _)| *name == "interrupt")
            .and_then(|(_, keys)| keys.into_iter().next())
            .unwrap_or_else(|| "ctrl+c".to_owned());
        self.renderer.show_notice(
            &format!("running… {interrupt_key} cancels"),
            Color::DarkGrey,
        );
        let sandbox = self.ui.sandbox.clone();
        let progress = crate::sandbox::CommandProgress::new();
        let mut progress_changes = progress.subscribe();
        let operation = sandbox.run_explicit_shell_with_progress(
            text,
            crate::sandbox::configured_command_limits(),
            Some(&cancellation),
            Some(progress.clone()),
        );
        tokio::pin!(operation);
        let mut progress_pending = false;
        let mut progress_painted: Option<tokio::time::Instant> = None;
        let run = loop {
            let progress_due = progress_painted.map(|painted| painted + BANG_PROGRESS_INTERVAL);
            tokio::select! {
                result = &mut operation => break result?,
                changed = progress_changes.changed(), if !progress_pending => {
                    if changed.is_ok() {
                        progress_pending = true;
                    }
                }
                _ = async {
                    if let Some(due) = progress_due {
                        tokio::time::sleep_until(due).await;
                    }
                }, if progress_pending => {
                    progress_pending = false;
                    progress_painted = Some(tokio::time::Instant::now());
                    let tail = progress.tail_lines(BANG_PROGRESS_LINES);
                    if !tail.is_empty()
                        && self.renderer.feed_mut().set_anchor_progress(
                            &progress_anchor,
                            crate::ui::feed::BlockStyle::ToolResult,
                            sanitize_output(&tail),
                        )
                    {
                        self.renderer.render_viewport()?;
                    }
                }
                event = self.user_rx.recv() => {
                    let Some(event) = event else {
                        cancellation.cancel();
                        break operation.await?;
                    };
                    match self.bang_live_event(event, &cancellation)? {
                        None => {}
                        Some(event) => {
                            if matches!(event, UserEvent::Quit) {
                                cancellation.cancel();
                            }
                            self.deferred_user_events.push_back(event);
                        }
                    }
                }
            }
        };
        if self
            .renderer
            .feed_mut()
            .clear_anchor_progress(&progress_anchor)
        {
            self.renderer.invalidate();
        }
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

        let color = if run.succeeded() { C_AGENT } else { C_ERROR };
        self.renderer.frame(|renderer| {
            for line in result.lines() {
                renderer.write_line(&sanitize_output(line), color)?;
            }
            renderer.write_line("", Color::White)
        })?;

        Ok(())
    }

    async fn mid_turn_compact(
        &mut self,
        pressure: f64,
        interactions: &[Message],
    ) -> anyhow::Result<()> {
        if self.run.native_owner.is_some() {
            let pending_steering = self.run.steering.clone();
            // A tool-reply boundary is not a completed new-user boundary.
            // Preserve the exact surfaced tail instead of silently dropping
            // signed history into an unbound compaction continuation.
            let prompt = self
                .run
                .pending_turn
                .as_ref()
                .map(|turn| turn.prompt().to_string());
            self.ui.session.adopt_provider_tool_identity(interactions);
            preserve_pending_main_turn_progress(&mut self.run, self.ui.session);
            self.run.is_running = false;
            self.run.settle(Duration::from_secs(5)).await?;
            self.run.steering = pending_steering.clone();
            super::settle_native_main_turn(
                &mut self.run,
                self.ui.session,
                crate::session::native_replay::RetiredNativeOutcome::LiveToolReply,
            )
            .await?;
            self.settle_success_transaction();
            self.discard_interrupted_queue();
            if let Some(prompt) = prompt {
                self.restore_interrupted_prompt(&prompt);
            }
            if let Some(steering) = pending_steering {
                self.restore_pending_steering(&steering)?;
            }
            self.run.steering = None;
            // The native epoch is closed at the tool-batch boundary, so the
            // saved transcript can be compacted and the thinking chain
            // restarted from it right away (the same path as between turns).
            // The restored prompt then continues on the compacted context
            // without /clear (mini-agent-tmysj).
            let reasoning_enabled = self.slash.reasoning_enabled;
            let compacted = crate::ui::slash::handle_compress_with_force(
                None,
                true,
                true,
                &mut self.run,
                &mut self.renderer,
                &mut self.ui,
                reasoning_enabled,
            )
            .await;
            let notice = match compacted {
                Ok(()) => {
                    "Context was full. Tool results are saved and the conversation was compacted; press Enter to continue on the compacted context."
                }
                Err(error) => {
                    self.renderer
                        .write_line(&format!("auto-compact error: {error:#}"), C_ERROR)?;
                    "Context is full. Tool results are saved; send a follow-up prompt to retry the compaction, or start a fresh context."
                }
            };
            self.renderer.write_line(notice, C_ERROR)?;
            return Ok(());
        }
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
                self.ui.cfg.custom_providers_map(),
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
                self.renderer.frame(|renderer| {
                    renderer.write_line(&format!("[btw #{}] answer:", id), C_BTW)?;
                    for line in response.lines() {
                        renderer.write_line(&sanitize_output(line), C_AGENT)?;
                    }
                    renderer.write_line("", Color::White)
                })?;
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
        // Startup skipped the overhead estimate when a prebuild was started;
        // show the measured preamble as soon as the agent lands.
        if let Some(snapshot) = self.run.agent.as_ref().and_then(AnyAgent::context_snapshot) {
            self.ui.session.overhead_tokens = snapshot.tokens;
            self.ui.session.overhead_parts = snapshot.parts.clone();
        }
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

    /// Ctrl+Z: give the terminal back, stop the whole process with SIGTSTP (the
    /// shell's `fg` continues it), then re-enter raw mode and the alternate
    /// screen and repaint. Same hand-off as an external editor or lazygit.
    #[cfg(unix)]
    fn suspend_to_shell(&mut self) -> anyhow::Result<()> {
        self.pause_event_thread();
        self.terminal_guard.suspend()?;
        let stopped = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP);
        // Execution continues here only after SIGCONT.
        let resume_result = self.terminal_guard.resume();
        self.rebind_event_thread();
        resume_result?;
        if let Err(error) = stopped {
            self.renderer
                .write_line(&format!("cannot suspend: {error}"), C_ERROR)?;
        }
        Ok(())
    }

    fn rebind_event_thread(&mut self) {
        self.pause_event_thread();
        // Terminal resumption clears the screen even when the draft is unchanged.
        self.renderer.invalidate();
        self.renderer.forget_title();
        // A suspension (editor, lazygit, pager) may have changed the
        // repository; refresh the Git statusline on the next iteration.
        self.loop_state
            .git_poll
            .mark_changed(std::time::Instant::now());
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
                    target: "mini_agent::audit::support_utility",
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
                    target: "mini_agent::audit::support_utility",
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
                    target: "mini_agent::audit::support_utility",
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
                    target: "mini_agent::audit::support_utility",
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
        match crate::session::storage::save_session_recovering(self.ui.session, |notice| {
            let _ = self
                .renderer
                .write_line(&format!("warning: {notice}"), C_ERROR);
        }) {
            Ok(()) => true,
            Err(error) => {
                if !crate::session::storage::is_session_size_error(&error) {
                    let _ = self.renderer.write_line(
                        &format!("warning: failed to save session: {error}"),
                        C_ERROR,
                    );
                }
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
        let Some((target, source)) = crate::extras::git_worktree::resolve_merge_target(&info).await
        else {
            let _ = self.renderer.write_line(
                &format!(
                    "worktree '{}' was not auto-merged: no target branch could be detected; run /wt-merge <branch>",
                    info.branch
                ),
                C_ERROR,
            );
            return Ok(());
        };
        let _ = self.renderer.write_line(
            &format!("auto-merge target '{target}' ({})", source.describe()),
            C_AGENT,
        );

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
                    crate::startup::heap_future(|| self.refresh_worktree_workspace_context())
                        .await?;
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
                if let Some(ss) = self.ui.status_signals.as_ref() {
                    ss.send_git_conflict();
                }
                // One frame, closed before the await below: the prompt is on
                // screen while it waits for the user.
                let _ = self.renderer.frame(|renderer| {
                    let _ = renderer.write_line(
                        &format!("merge conflict in {} file(s):", files.len()),
                        C_ERROR,
                    );
                    for f in &files {
                        let _ = renderer.write_line(&format!("  {}", f), C_ERROR);
                    }
                    renderer.write_line("[a]bort  [l]eave for manual resolution", C_PERM)
                });

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
                            crate::startup::heap_future(|| {
                                self.refresh_worktree_workspace_context()
                            })
                            .await?;
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
                            crate::startup::heap_future(|| {
                                self.refresh_worktree_workspace_context()
                            })
                            .await?;
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
            self.mark_startup_skill_commands_stale();
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
#[path = "app_tests/initial_turn_tests.rs"]
mod initial_turn_tests;

#[cfg(test)]
#[path = "app_tests/mid_turn_pressure_tests.rs"]
mod mid_turn_pressure_tests;

/// Release a saturated input sender without closing the shared completion
/// channel. Preserve drained events in their original order for the next UI
/// iteration. Once stopped, the reader can finish at most its current event.
fn pause_input_reader(
    running: &AtomicBool,
    handle: &mut Option<super::InputReader>,
    receiver: &mut mpsc::Receiver<UserEvent>,
    deferred: &mut std::collections::VecDeque<UserEvent>,
) {
    /// How often a wait re-drains the queue for a reader blocked on a full
    /// channel; the reader's own exit wakes the wait at once.
    const DRAIN_INTERVAL: Duration = Duration::from_millis(5);
    running.store(false, Ordering::Relaxed);
    let Some(reader) = handle.take() else {
        return;
    };
    let mut wait = || {
        loop {
            // Free the queue so the reader's current send can complete, even
            // when background producers refill it meanwhile.
            while let Ok(event) = receiver.try_recv() {
                deferred.push_back(event);
            }
            if reader.wait_finished(DRAIN_INTERVAL) {
                break;
            }
        }
        while let Ok(event) = receiver.try_recv() {
            deferred.push_back(event);
        }
    };
    // The wait (at most one reader poll) must not stall other tasks queued
    // on this worker of a multi-threaded runtime.
    #[cfg(feature = "multithread")]
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|runtime| runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(wait);
        reader.join();
        return;
    }
    wait();
    reader.join();
}

/// Slash commands that read the tty synchronously while the terminal is
/// suspended: `/tutor` runs a pager, `/init` (without `force`) asks y/N on
/// stdin. The event thread must be paused around them.
fn slash_command_needs_tty(text: &str) -> bool {
    let mut parts = text.split_whitespace();
    match parts.next() {
        Some("/config") => crate::ui::slash::config::needs_tty(text),
        Some("/tutor" | "/login") => true,
        Some("/init") => parts.next() != Some("force"),
        _ => false,
    }
}

#[cfg(test)]
#[path = "app_tests/slash_tty_tests.rs"]
mod slash_tty_tests;

#[cfg(test)]
#[path = "app_tests/input_reader_lifecycle_tests.rs"]
mod input_reader_lifecycle_tests;

#[cfg(test)]
#[path = "app_tests/interrupt_rewind_tests.rs"]
mod interrupt_rewind_tests;

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
            native_startup: None,
            native_ticket: None,
            #[cfg(feature = "hooks")]
            pending_session_start_context: None,
            cli,
            cfg,
            session,
            context,
            workspace,
            client: crate::provider::create_client(
                "openrouter",
                Some("unused-test-key"),
                cfg.custom_providers_map(),
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
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "MINI_AGENT_CREDENTIALS_DIR",
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

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_shell_live_tail_is_replaced_by_the_final_output() {
        let root = crate::tests::OwnedTestDirectory::new("explicit-shell-live-tail");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "MINI_AGENT_CREDENTIALS_DIR",
                Some(root.path().join("credentials").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_session: true,
            no_sandbox: true,
            tools: vec!["shell".into()],
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("openrouter", "fixture", 128_000, "");
        let _session_release = SessionRelease(session.id.to_string());
        let ui = shell_ui(&cli, &cfg, &mut session, &mut context, workspace);
        let mut app = fixture_app(ui);
        let blocks_before = app.renderer.feed().block_count();
        app.run_bang_command("!printf 'EARLY_LINE\\n'; sleep 0.5; printf 'LATE_LINE\\n'")
            .await
            .unwrap();
        let lines = app.renderer.feed().line_count(4096);
        let shown = app
            .renderer
            .feed()
            .selected_text(4096, 0, lines - 1)
            .unwrap();
        assert_eq!(shown.matches("EARLY_LINE").count(), 2, "{shown}");
        assert_eq!(shown.matches("LATE_LINE").count(), 2, "{shown}");
        // Echoed command, anchored separator, two output lines, trailing blank.
        let texts: Vec<_> = (blocks_before..app.renderer.feed().block_count())
            .map(|index| app.renderer.feed().block_text(index).unwrap().to_owned())
            .collect();
        assert!(
            !texts
                .iter()
                .any(|text| text.contains("EARLY_LINE\nLATE_LINE")),
            "the live tail block is removed after the run: {texts:?}"
        );
        assert_eq!(texts.iter().filter(|text| *text == "EARLY_LINE").count(), 1);
    }

    #[tokio::test]
    async fn chain_plan_and_git_prompts_discard_type_ahead_until_armed() {
        use std::time::Duration;
        let root = crate::tests::OwnedTestDirectory::new("chain-prompt-arming");
        let cli = crate::cli::Cli {
            no_session: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("openrouter", "fixture", 128_000, "");
        let ui = shell_ui(&cli, &cfg, &mut session, &mut context, workspace);
        let mut app = fixture_app(ui);
        let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        // No prompt: nothing is premature.
        assert!(!app.chain_prompt_premature_with(y, Duration::from_secs(60)));
        // A freshly opened prompt (plan approval, git proposal and the chain
        // prompt all share `ChainPrompt::new`) ignores answer keys until armed.
        app.renderer.chain_prompt = Some(ChainPrompt::new("approve?"));
        assert!(app.chain_prompt_premature_with(y, Duration::from_secs(60)));
        assert!(!app.chain_prompt_premature_with(y, Duration::ZERO));
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.chain_prompt_premature_with(esc, Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn an_open_picker_is_not_drawn_over_a_permission_prompt() {
        use crate::ui::renderer::PermissionPrompt;
        let root = crate::tests::OwnedTestDirectory::new("picker-under-prompt");
        let cli = crate::cli::Cli {
            no_session: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("openrouter", "fixture", 128_000, "");
        let ui = shell_ui(&cli, &cfg, &mut session, &mut context, workspace);
        let mut app = fixture_app(ui);
        app.input.start_command_picker();
        app.refresh().unwrap();
        assert!(app.renderer.captured_output().contains("/agent"));

        app.renderer.permission_prompt = Some(PermissionPrompt {
            tool: "shell".into(),
            options: "[y] allow".into(),
            expanded: false,
        });
        let before = app.renderer.captured_output().len();
        app.refresh().unwrap();
        let during = app.renderer.captured_output()[before..].to_owned();
        assert!(!during.contains("/agent"), "picker drawn over prompt");
        assert!(app.input.picker.as_ref().is_some_and(|p| p.active()));

        app.renderer.permission_prompt = None;
        let before = app.renderer.captured_output().len();
        app.refresh().unwrap();
        assert!(app.renderer.captured_output()[before..].contains("/agent"));
    }

    #[tokio::test]
    async fn typed_keys_edit_the_draft_while_a_bang_command_runs() {
        let root = crate::tests::OwnedTestDirectory::new("bang-live-typing");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "MINI_AGENT_CREDENTIALS_DIR",
                Some(root.path().join("credentials").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_session: true,
            no_sandbox: true,
            tools: vec!["shell".into()],
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("openrouter", "fixture", 128_000, "");
        let _session_release = SessionRelease(session.id.to_string());
        let ui = shell_ui(&cli, &cfg, &mut session, &mut context, workspace);
        let mut app = fixture_app(ui);
        app.user_tx
            .try_send(UserEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        app.run_bang_command("!sleep 0.5; printf DONE")
            .await
            .unwrap();
        assert_eq!(
            app.input.buffer.as_str(),
            "x",
            "typed key reaches the draft"
        );
        assert!(
            app.deferred_user_events.is_empty(),
            "an editing key is not parked until the command ends"
        );
    }

    #[tokio::test]
    async fn a_command_refused_while_running_stays_in_the_draft() {
        let root = crate::tests::OwnedTestDirectory::new("reject-keeps-draft");
        let cli = crate::cli::Cli {
            no_session: true,
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("openrouter", "fixture", 128_000, "");
        let ui = shell_ui(&cli, &cfg, &mut session, &mut context, workspace);
        let mut app = fixture_app(ui);
        app.run.is_running = true;
        app.input.load_text("/model x");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_key_event_with(enter, None).await.unwrap();
        assert_eq!(app.input.buffer.as_str(), "/model x");
    }

    #[tokio::test]
    async fn actual_explicit_shell_save_failure_keeps_live_record_and_warns() {
        let root = crate::tests::OwnedTestDirectory::new("explicit-shell-save-failure");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "MINI_AGENT_CREDENTIALS_DIR",
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
#[path = "app_tests/permission_loop_tests.rs"]
mod permission_loop_tests;

#[cfg(test)]
#[path = "app_tests/mutation_consumer_tests.rs"]
mod mutation_consumer_tests;

#[cfg(test)]
#[path = "app_tests/compaction_visibility_tests.rs"]
mod compaction_visibility_tests;

#[cfg(test)]
#[path = "app_tests/custom_command_tests.rs"]
mod custom_command_tests;

#[cfg(test)]
#[path = "app_tests/workspace_session_picker_tests.rs"]
mod workspace_session_picker_tests;

#[cfg(test)]
#[path = "app_tests/usd_budget_app_tests.rs"]
mod usd_budget_app_tests;

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
                cfg.custom_providers_map(),
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
                cfg.custom_providers_map(),
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
        assert_eq!(body["max_tokens"], crate::provider::GIT_PROPOSAL_MAX_TOKENS);
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
        let mut wire =
            crate::provider::git_proposal_test_support::Wire::new(r#"{"paths":["unrelated.txt"]}"#);
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
                cfg.custom_providers_map(),
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
                cfg.custom_providers_map(),
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
        assert_eq!(body["max_tokens"], crate::provider::GIT_PROPOSAL_MAX_TOKENS);
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
    /// On a direct Anthropic session a running goal refuses the next message.
    /// That must be an inline notice that keeps the input, not an error that
    /// unwinds to the event loop (mini-agent-sskdu.203).
    #[cfg(feature = "goal")]
    #[tokio::test]
    async fn a_native_session_with_a_running_goal_refuses_a_message_inline() {
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
        session
            .goal_store
            .set(
                crate::extras::goal::Goal::new("keep going", Vec::new()).unwrap(),
                false,
            )
            .unwrap();
        let mut files = crate::context::load(true).for_workspace_binding(true, &repo.workspace);
        let mut app = fixture_app(context(&cli, &cfg, &mut session, &mut files, &repo));
        let gateway: crate::config::Config = serde_json::from_value(serde_json::json!({
            "custom_providers": {"gw": {"provider_type": "anthropic", "base_url": "http://127.0.0.1:9"}}
        }))
        .unwrap();
        app.ui.client = crate::provider::create_client(
            "gw",
            Some("unused-local-key"),
            gateway.custom_providers_map(),
            None,
        )
        .unwrap();

        app.start_main_run("hello", crate::question::QuestionOrigin::NewUserTurn)
            .await
            .expect("a refusal is a notice, not an error");
        assert!(!app.run.is_running);
        assert!(
            app.renderer
                .captured_output()
                .contains("direct Anthropic route"),
            "the user is told why"
        );
        assert_eq!(app.input.buffer, "hello", "the message stays in the editor");

        // A parked goal does not block chat.
        app.ui.session.goal_store.with_mut(|goal| {
            goal.set_status(
                crate::extras::goal::GoalStatus::Paused,
                Some(crate::extras::goal::PauseReason::UserRequested),
            )
        });
        assert!(!super::super::native_goal_blocks_start(&app.ui));
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
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "MINI_AGENT_CREDENTIALS_DIR",
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
                cfg.custom_providers_map(),
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
                .filter(|text| text.contains("git command requires the 'git-worktree' feature"))
                .count(),
            3
        );
        assert!(app.ui.session.messages.is_empty());
    }
}

#[cfg(test)]
#[path = "app_tests/context_diagnostic_tests.rs"]
mod context_diagnostic_tests;

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
                            "MINI_AGENT_CONFIG_DIR",
                            "MINI_AGENT_DATA_DIR",
                            "MINI_AGENT_LOCAL_DATA_DIR",
                            "MINI_AGENT_STATE_DIR",
                            "MINI_AGENT_CACHE_DIR",
                            "MINI_AGENT_CREDENTIALS_DIR",
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
                            cfg.custom_providers_map(),
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

/// The discovery diagnostic that rejected the skill package directory `name`.
#[cfg(feature = "agent-skills")]
fn rejected_skill_reason(diagnostics: &[String], name: &str) -> Option<String> {
    let marker = format!(" skill '{name}' rejected: ");
    diagnostics
        .iter()
        .find(|diagnostic| diagnostic.contains(&marker))
        .cloned()
}

#[cfg(feature = "agent-skills")]
fn skill_command_entries(
    snapshot: &crate::extras::skills::discovery::Discovery,
) -> Vec<(String, String)> {
    snapshot
        .packages
        .iter()
        .filter(|(name, package)| {
            package.record.user_invocable && !crate::ui::pickers::list::reserved_command(name)
        })
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
        .collect()
}

#[cfg_attr(not(feature = "agent-skills"), allow(dead_code))]
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
pub(crate) async fn drive_native_steer_fixture(
    ui: UiContext<'_>,
    stop_at_boundary: bool,
) -> Session {
    plan_approval_tests::drive_native_steer_fixture(ui, stop_at_boundary).await
}

#[cfg(all(test, feature = "subagents"))]
pub(crate) async fn drive_native_background_fixture(
    ui: UiContext<'_>,
    child: crate::extras::subagents::SubagentConfig,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
) -> Vec<Session> {
    plan_approval_tests::drive_native_background_fixture(ui, child, calls, release).await
}

#[cfg(test)]
pub(crate) async fn drive_native_fixture_turn(ui: UiContext<'_>, prompt: &str) {
    plan_approval_tests::drive_native_fixture_turns(ui, &[prompt], false).await;
}

#[cfg(test)]
pub(crate) async fn drive_native_fixture_turns(
    ui: UiContext<'_>,
    prompts: &[&str],
) -> Vec<Session> {
    plan_approval_tests::drive_native_fixture_turns(ui, prompts, false).await
}

#[cfg(all(test, feature = "hooks", feature = "agent-skills"))]
pub(crate) async fn drive_native_stale_session_end_fixture(ui: UiContext<'_>) -> Vec<Session> {
    plan_approval_tests::drive_native_fixture_turns(
        ui,
        &["first literal TUI prompt", "second literal TUI prompt"],
        true,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn drive_native_script(
    ui: UiContext<'_>,
    steps: &[String],
    observe: bool,
) -> Vec<Session> {
    plan_approval_tests::drive_native_script(ui, steps, observe).await
}

#[cfg(test)]
pub(crate) async fn drive_native_title_fixture(
    ui: UiContext<'_>,
    prompts: &[String],
) -> Vec<Session> {
    plan_approval_tests::drive_native_title_fixture(ui, prompts).await
}

#[cfg(test)]
pub(crate) async fn drive_native_effort_fixture(ui: UiContext<'_>) -> Vec<Session> {
    plan_approval_tests::drive_native_effort_fixture(ui).await
}

#[cfg(all(test, feature = "agent-skills"))]
pub(crate) async fn drive_native_instruction_fixture(
    ui: UiContext<'_>,
    package: &std::path::Path,
) -> Vec<Session> {
    plan_approval_tests::drive_native_instruction_fixture(ui, package).await
}

#[cfg(test)]
#[path = "app_tests/plan_approval_tests.rs"]
mod plan_approval_tests;

#[cfg(test)]
#[path = "app_tests/config_editor68_tests.rs"]
mod config_editor68_tests;

#[cfg(all(test, feature = "agent-skills"))]
mod rejected_skill_tests {
    use super::rejected_skill_reason;

    #[test]
    fn an_unknown_slash_word_finds_the_rejection_of_its_package() {
        let diagnostics = vec![
            "project skill 'other' rejected: bad front matter".to_string(),
            "project skill 'release' rejected: missing description".to_string(),
        ];
        assert_eq!(
            rejected_skill_reason(&diagnostics, "release").as_deref(),
            Some("project skill 'release' rejected: missing description")
        );
        assert_eq!(rejected_skill_reason(&diagnostics, "absent"), None);
    }
}

/// Whether the interactive start may show the ARCHITECTURE.md offer: context
/// files are enabled and `architecture_prompt` was not turned off.
#[cfg(feature = "archmd")]
fn interactive_arch_offer_allowed(ui: &UiContext<'_>) -> bool {
    !ui.cli.resolve_no_context_files(ui.cfg) && ui.cfg.resolve_architecture_prompt()
}
