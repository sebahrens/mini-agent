use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Color;

use crate::config::keybindings::KeyAction;
use crate::event::UserEvent;
use crate::permission::checker::PromptGrantOffer;
use crate::ui::events::sanitize_for_review;
use crate::ui::renderer::Renderer;
use crate::ui::state::{AgentRunState, UiContext};
use crate::ui::utils::suggest_pattern;

use super::{C_ERROR, C_PERM};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptInput {
    AllowOnce,
    /// Allow the suggested pattern for the rest of this session.
    AllowSession,
    /// Allow read, edit and list_dir on the request's folder for the session.
    AllowFolder,
    /// Only native write/edit under the captured workspace, for this session.
    AllowWorkspaceEdits,
    Deny,
    /// Esc / Ctrl+C / Ctrl+D: deny and stop the current run.
    Abort,
    StopWithInstruction,
    /// `e`: expand or collapse the prompt's view of the request. Never an
    /// answer.
    ToggleFull,
    Ignore,
}

/// Options line for an ordinary approval. Session-scoped grants are labelled
/// as such: nothing here persists beyond the session.
fn permission_options(folder: Option<&std::path::Path>) -> String {
    match folder {
        Some(folder) => format!(
            "  (y) allow once  (a) allow for this session  (f) allow read+edit+list in {} for this session  (n) deny once  (s) deny and stop with instruction  (ESC) deny and stop",
            folder.display()
        ),
        None => "  (y) allow once  (a) allow for this session  (n) deny once  (s) deny and stop with instruction  (ESC) deny and stop".to_string(),
    }
}

fn prompt_grant_header(offer: &PromptGrantOffer) -> String {
    format!(
        "[permission] prompt '{}' asks for {} access to {} for this session",
        offer.prompt,
        offer.tools.join(", "),
        offer.dir.display()
    )
}

const PROMPT_GRANT_OPTIONS: &str = "  (y) allow for this session  (n) ask for each access instead  (s) deny and stop with instruction  (ESC) deny and stop";

fn classify_prompt_key(key: KeyEvent) -> PromptInput {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C' | 'd' | 'D'))
    {
        return PromptInput::Abort;
    }
    let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Char('y' | 'Y') if plain => PromptInput::AllowOnce,
        KeyCode::Char('a' | 'A') if plain => PromptInput::AllowSession,
        KeyCode::Char('f' | 'F') if plain => PromptInput::AllowFolder,
        KeyCode::Char('w' | 'W') if plain => PromptInput::AllowWorkspaceEdits,
        KeyCode::Char('n' | 'N') if plain => PromptInput::Deny,
        KeyCode::Char('s' | 'S') if plain => PromptInput::StopWithInstruction,
        KeyCode::Char('e' | 'E') if plain => PromptInput::ToggleFull,
        KeyCode::Esc => PromptInput::Abort,
        _ => PromptInput::Ignore,
    }
}

/// Transcript navigation that stays available while a prompt waits, so the
/// request's context (and a long command's full text) can be reviewed
/// before answering. The prompt claims none of these keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PromptScroll {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

pub(super) fn prompt_scroll(event: &UserEvent) -> Option<PromptScroll> {
    match event {
        UserEvent::ScrollUp => Some(PromptScroll::LineUp),
        UserEvent::ScrollDown => Some(PromptScroll::LineDown),
        UserEvent::ScrollPageUp => Some(PromptScroll::PageUp),
        UserEvent::ScrollPageDown => Some(PromptScroll::PageDown),
        // The shared keybindings layer owns the page and jump chords (PageUp,
        // Alt+Up, Ctrl+Home, Alt+<, ...), so the prompt never drifts from the
        // idle prompt. Prompts use the default table, as `keyboard_action` does.
        UserEvent::Key(key) => {
            match crate::config::keybindings::KeyBindings::default().action(*key, false) {
                Some(KeyAction::PageUp) => Some(PromptScroll::PageUp),
                Some(KeyAction::PageDown) => Some(PromptScroll::PageDown),
                Some(KeyAction::TranscriptTop) => Some(PromptScroll::Top),
                Some(KeyAction::TranscriptBottom) => Some(PromptScroll::Bottom),
                _ => match key.code {
                    KeyCode::Up => Some(PromptScroll::LineUp),
                    KeyCode::Down => Some(PromptScroll::LineDown),
                    KeyCode::Home => Some(PromptScroll::Top),
                    KeyCode::End => Some(PromptScroll::Bottom),
                    _ => None,
                },
            }
        }
        _ => None,
    }
}

pub(super) fn apply_prompt_scroll(
    renderer: &mut Renderer,
    scroll: PromptScroll,
) -> std::io::Result<()> {
    match scroll {
        PromptScroll::LineUp => renderer.scroll_line_up(),
        PromptScroll::LineDown => renderer.scroll_line_down(),
        PromptScroll::PageUp => renderer.scroll_page_up(),
        PromptScroll::PageDown => renderer.scroll_page_down(),
        PromptScroll::Top => renderer.scroll_to_top(),
        PromptScroll::Bottom => renderer.scroll_to_bottom()?,
    }
    Ok(())
}

/// Paint the transcript and the prompt for the current geometry. The input
/// height is reconciled first so the transcript is sized around the
/// prompt's (possibly multi-row) layout. The statusline keeps showing what
/// the last frame built (model, mode, context), not blank rows.
pub(super) fn draw_prompt(renderer: &mut Renderer) -> std::io::Result<()> {
    renderer.sync_input_height("")?;
    renderer.render_viewport()?;
    let (statusline_key, statusline) = renderer.last_statusline();
    renderer.draw_bottom("", 0, &statusline, statusline_key, false)
}

/// The main App loop owns this state; no permission handler waits on a
/// receiver. Dropping an unanswered state explicitly denies its exact request.
pub(crate) struct PermissionState {
    origin_valid: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
    #[cfg(feature = "mcp")]
    origin_owner: Option<crate::extras::mcp::elicitation::FrontendOwner>,
    request: Option<crate::permission::ask::AskRequest>,
    offer: Option<PromptGrantOffer>,
    folder: Option<std::path::PathBuf>,
    workspace_edits: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    was_at_bottom: bool,
    instruction: Option<String>,
}

pub(crate) enum PermissionEvent {
    Unhandled,
    Waiting,
    Finished,
    Stop { instruction: bool },
    Instruction(String),
}

impl Drop for PermissionState {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            let _ = request
                .reply
                .send(crate::permission::ask::UserDecision::Deny);
        }
    }
}

impl PermissionState {
    pub(crate) fn begin(
        request: crate::permission::ask::AskRequest,
        renderer: &mut Renderer,
        ui: &UiContext<'_>,
        run: &mut AgentRunState,
    ) -> anyhow::Result<Self> {
        run.was_reasoning = false;
        if run.agent_line_started {
            renderer.write_line("", Color::White)?;
            run.agent_line_started = false;
        }
        let (offer, folder) = ui.permission.as_ref().map_or((None, None), |permission| {
            let guard = permission.lock().unwrap_or_else(|error| error.into_inner());
            (
                guard.prompt_grant_offer_for(&request.tool, &request.input),
                guard.folder_grant_scope(&request.tool, &request.input),
            )
        });
        let workspace_edits = ui.permission.as_ref().and_then(|permission| {
            permission
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .can_offer_workspace_edits(&request.tool, &request.input, &ui.workspace)
                .then(|| ui.workspace.clone())
        });
        let state = Self {
            origin_valid: None,
            #[cfg(feature = "mcp")]
            origin_owner: None,
            request: Some(request),
            offer,
            folder,
            workspace_edits,
            was_at_bottom: !renderer.is_scrolling(),
            instruction: None,
        };
        state.show(renderer)?;
        renderer.set_activity(crate::ui::terminal::AgentActivity::WaitingForApproval)?;
        Ok(state)
    }

    /// Collect a next-turn draft only after the App has retired the denied run.
    pub(crate) fn instruction(renderer: &mut Renderer) -> anyhow::Result<Self> {
        let state = Self {
            origin_valid: None,
            #[cfg(feature = "mcp")]
            origin_owner: None,
            request: None,
            offer: None,
            folder: None,
            workspace_edits: None,
            was_at_bottom: !renderer.is_scrolling(),
            instruction: Some(String::new()),
        };
        state.show_instruction(renderer)?;
        Ok(state)
    }

    #[cfg(feature = "subagents")]
    pub(crate) fn background_for(&self, session: &crate::session::Session) -> bool {
        self.request
            .as_ref()
            .and_then(|r| r.background.as_ref())
            .is_some_and(|origin| origin.valid(session))
    }
    #[cfg(feature = "subagents")]
    pub(crate) fn stale_background(&self, session: &crate::session::Session) -> bool {
        self.request
            .as_ref()
            .and_then(|r| r.background.as_ref())
            .is_some_and(|origin| !origin.valid(session))
    }
    /// `revalidate == false` skips the bound origin's source revalidation and
    /// checks only the cheap reply-channel state; replies stay authoritative.
    pub(crate) fn abandoned_with(&self, revalidate: bool) -> bool {
        (revalidate && self.origin_valid.as_ref().is_some_and(|valid| !valid()))
            || self
                .request
                .as_ref()
                .is_some_and(|request| request.reply.is_closed())
    }
    #[cfg(feature = "mcp")]
    pub(crate) fn bind_origin(&mut self, valid: std::sync::Arc<dyn Fn() -> bool + Send + Sync>) {
        self.origin_valid = Some(valid);
    }
    #[cfg(feature = "mcp")]
    pub(crate) fn bind_owner(&mut self, owner: crate::extras::mcp::elicitation::FrontendOwner) {
        self.origin_owner = Some(owner);
    }
    #[cfg(feature = "mcp")]
    pub(crate) fn origin_owner(&self) -> Option<&crate::extras::mcp::elicitation::FrontendOwner> {
        self.origin_owner.as_ref()
    }

    pub(crate) fn decline_on_cancel(&self, ui: &UiContext<'_>) {
        if self.offer.is_some()
            && let Some(permission) = &ui.permission
        {
            permission
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .decline_prompt_grant();
        }
    }

    pub(crate) fn finish(&self, renderer: &mut Renderer) -> std::io::Result<()> {
        renderer.permission_prompt = None;
        if self.was_at_bottom {
            renderer.scroll_to_bottom()?;
        }
        Ok(())
    }

    fn show(&self, renderer: &mut Renderer) -> anyhow::Result<()> {
        let request = self.request.as_ref().expect("approval owns its request");
        let (header, mut options) = match &self.offer {
            Some(offer) => (prompt_grant_header(offer), PROMPT_GRANT_OPTIONS.to_string()),
            None => (
                format!("[permission] {}: {}", request.tool, request.input),
                permission_options(self.folder.as_deref()),
            ),
        };
        if let Some(workspace) = &self.workspace_edits {
            options.push_str(&format!(
                "  (w) allow writes/edits in {} for this session",
                workspace.root().display()
            ));
        }
        let header = match &request.agent_name {
            Some(name) => format!("[subagent: {name}] {header}"),
            None => header,
        };
        let header = match request.diagnostic {
            Some(diagnostic) => format!("{header} [{diagnostic}]"),
            None => header,
        };
        let header = sanitize_for_review(&header);
        let options = sanitize_for_review(&options);
        renderer.write_line(&header, C_PERM)?;
        renderer.write_line(&options, C_PERM)?;
        renderer.permission_prompt = Some(super::renderer::PermissionPrompt {
            tool: header,
            options,
            expanded: false,
        });
        draw_prompt(renderer)?;
        Ok(())
    }

    fn show_instruction(&self, renderer: &mut Renderer) -> anyhow::Result<()> {
        renderer.permission_prompt = Some(super::renderer::PermissionPrompt {
            tool: format!(
                "Run stopped. Next user instruction (draft): {}",
                sanitize_for_review(self.instruction.as_deref().unwrap_or_default())
            )
            .into(),
            options: "[Enter] put instruction in editor; review before sending  [Esc] skip".into(),
            expanded: true,
        });
        draw_prompt(renderer)?;
        Ok(())
    }

    pub(crate) fn handle_event(
        &mut self,
        event: &UserEvent,
        renderer: &mut Renderer,
        ui: &mut UiContext<'_>,
        run: &mut AgentRunState,
        deferred: &mut VecDeque<UserEvent>,
    ) -> anyhow::Result<PermissionEvent> {
        use crate::permission::ask::UserDecision;
        if let Some(scroll) = prompt_scroll(event) {
            apply_prompt_scroll(renderer, scroll)?;
            draw_prompt(renderer)?;
            return Ok(PermissionEvent::Waiting);
        }
        if matches!(event, UserEvent::Resize) {
            renderer.resize();
            renderer.invalidate();
            draw_prompt(renderer)?;
            return Ok(PermissionEvent::Waiting);
        }
        if let Some(instruction) = self.instruction.as_mut() {
            match event {
                UserEvent::Key(key) => match key.code {
                    KeyCode::Enter => {
                        let text = instruction.trim().to_string();
                        self.finish(renderer)?;
                        return Ok(PermissionEvent::Instruction(text));
                    }
                    KeyCode::Esc => {
                        self.finish(renderer)?;
                        return Ok(PermissionEvent::Finished);
                    }
                    _ if classify_prompt_key(*key) == PromptInput::Abort => {
                        self.finish(renderer)?;
                        return Ok(PermissionEvent::Finished);
                    }
                    KeyCode::Backspace => {
                        instruction.pop();
                    }
                    KeyCode::Char(character)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            && !character.is_control() =>
                    {
                        instruction.push(character)
                    }
                    _ => {}
                },
                UserEvent::Paste(text) => {
                    // One line; a paste cannot submit or replay queued input.
                    instruction.extend(text.chars().filter(|character| !character.is_control()));
                }
                _ => return Ok(PermissionEvent::Unhandled),
            }
            self.show_instruction(renderer)?;
            return Ok(PermissionEvent::Waiting);
        }
        let input = match event {
            UserEvent::Key(key) => classify_prompt_key(*key),
            UserEvent::Paste(text) => {
                deferred.push_back(UserEvent::Paste(text.clone()));
                return Ok(PermissionEvent::Waiting);
            }
            _ => return Ok(PermissionEvent::Unhandled),
        };
        if input == PromptInput::ToggleFull {
            if let Some(prompt) = renderer.permission_prompt.as_mut() {
                prompt.expanded = !prompt.expanded;
            }
            draw_prompt(renderer)?;
            return Ok(PermissionEvent::Waiting);
        }
        if input == PromptInput::Ignore
            || (input == PromptInput::AllowFolder && self.folder.is_none())
            || (input == PromptInput::AllowWorkspaceEdits && self.workspace_edits.is_none())
            || (self.offer.is_some()
                && matches!(input, PromptInput::AllowSession | PromptInput::AllowFolder))
        {
            return Ok(PermissionEvent::Waiting);
        }
        if input == PromptInput::AllowWorkspaceEdits {
            // A stale request or replaced workspace cannot change session state.
            let request = self.request.take().expect("approval owns its request");
            let entries = match (&self.workspace_edits, &ui.permission) {
                (Some(workspace), Some(permission)) => {
                    let mut checker = permission.lock().unwrap_or_else(|error| error.into_inner());
                    if run.is_running
                        && !request.reply.is_closed()
                        && checker.can_offer_workspace_edits(
                            &request.tool,
                            &request.input,
                            workspace,
                        )
                    {
                        if request.reply.send(UserDecision::AllowOnce).is_ok() {
                            checker.add_workspace_edit_grant(workspace)?
                        } else {
                            Vec::new()
                        }
                    } else {
                        let _ = request.reply.send(UserDecision::Deny);
                        Vec::new()
                    }
                }
                _ => {
                    let _ = request.reply.send(UserDecision::Deny);
                    Vec::new()
                }
            };
            self.finish(renderer)?;
            record_session_grants(entries, renderer, ui, run)?;
            return Ok(PermissionEvent::Finished);
        }
        if matches!(input, PromptInput::Abort | PromptInput::StopWithInstruction) {
            if self.offer.is_some()
                && let Some(permission) = &ui.permission
            {
                permission
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .decline_prompt_grant();
            }
            if let Some(request) = self.request.take() {
                let _ = request.reply.send(UserDecision::Deny);
            }
            self.finish(renderer)?;
            return Ok(PermissionEvent::Stop {
                instruction: input == PromptInput::StopWithInstruction,
            });
        }
        if self.offer.take().is_some() {
            let Some(permission) = &ui.permission else {
                self.finish(renderer)?;
                return Ok(PermissionEvent::Finished);
            };
            if input == PromptInput::Deny {
                permission
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .decline_prompt_grant();
                self.show(renderer)?;
                return Ok(PermissionEvent::Waiting);
            }
            let request = self.request.take().expect("approval owns its request");
            let entries = if request.reply.send(UserDecision::AllowOnce).is_ok() {
                permission
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .accept_prompt_grant()
            } else {
                permission
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .decline_prompt_grant();
                Vec::new()
            };
            self.finish(renderer)?;
            record_session_grants(entries, renderer, ui, run)?;
            return Ok(PermissionEvent::Finished);
        }
        let request = self.request.take().expect("approval owns its request");
        if input == PromptInput::AllowFolder {
            let entries = if request.reply.send(UserDecision::AllowOnce).is_ok() {
                match (&self.folder, &ui.permission) {
                    (Some(folder), Some(permission)) => permission
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .add_session_folder_grant(
                            folder,
                            &crate::permission::checker::FOLDER_GRANT_TOOLS,
                        ),
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            self.finish(renderer)?;
            record_session_grants(entries, renderer, ui, run)?;
            return Ok(PermissionEvent::Finished);
        }
        let (decision, entries) = match input {
            PromptInput::AllowOnce => (UserDecision::AllowOnce, Vec::new()),
            PromptInput::AllowSession => {
                let pattern = request
                    .suggested_pattern
                    .clone()
                    .unwrap_or_else(|| suggest_pattern(&request.tool, &request.input));
                renderer.write_line(&format!("  -> will allow: {}", pattern), Color::Green)?;
                let mut entries = vec![(request.tool.to_string(), pattern.clone())];
                entries.extend(
                    request
                        .additional_allow_patterns
                        .iter()
                        .map(|extra| (request.tool.to_string(), extra.clone())),
                );
                (UserDecision::AllowAlways(pattern), entries)
            }
            _ => (UserDecision::Deny, Vec::new()),
        };
        let entries = if request.reply.send(decision).is_ok() {
            entries
        } else {
            Vec::new()
        };
        self.finish(renderer)?;
        record_session_grants(entries, renderer, ui, run)?;
        Ok(PermissionEvent::Finished)
    }
}

/// Report and persist session grants. The checker already holds them (the
/// agent side adds `AllowAlways` patterns; folder grants are added above), so
/// this only records them for session resume.
fn record_session_grants(
    entries: Vec<(String, String)>,
    renderer: &mut Renderer,
    ui: &mut UiContext<'_>,
    run: &mut AgentRunState,
) -> anyhow::Result<()> {
    for (tool, pattern) in entries {
        renderer.write_line(
            &format!("  allowed {} {} for this session", tool, pattern),
            Color::Green,
        )?;
        ui.session
            .permission_allowlist
            .push(crate::session::PermissionAllowEntry {
                tool: tool.into(),
                pattern: pattern.into(),
            });
        if let Err(e) = crate::ui::persist_session_if_settled(ui.session, !ui.cli.no_session, run) {
            renderer.write_line(&format!("warning: failed to save session: {}", e), C_ERROR)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_c_and_d_abort_permission_prompt() {
        for code in [KeyCode::Char('c'), KeyCode::Char('d')] {
            assert_eq!(
                classify_prompt_key(KeyEvent::new(code, KeyModifiers::CONTROL)),
                PromptInput::Abort
            );
        }
    }

    #[test]
    fn modified_allow_keys_cannot_approve_a_request() {
        assert_eq!(
            classify_prompt_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            PromptInput::Ignore
        );
        assert_eq!(
            classify_prompt_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::ALT)),
            PromptInput::Ignore
        );
    }

    #[test]
    fn the_transcript_scrolls_while_a_prompt_waits() {
        let key = |code| UserEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(
            prompt_scroll(&UserEvent::ScrollUp),
            Some(PromptScroll::LineUp)
        );
        assert_eq!(
            prompt_scroll(&UserEvent::ScrollDown),
            Some(PromptScroll::LineDown)
        );
        assert_eq!(
            prompt_scroll(&key(KeyCode::PageUp)),
            Some(PromptScroll::PageUp)
        );
        assert_eq!(
            prompt_scroll(&key(KeyCode::PageDown)),
            Some(PromptScroll::PageDown)
        );
        assert_eq!(prompt_scroll(&key(KeyCode::Home)), Some(PromptScroll::Top));
        assert_eq!(
            prompt_scroll(&key(KeyCode::End)),
            Some(PromptScroll::Bottom)
        );
        // Answer keys are never swallowed by navigation.
        for answer in ['y', 'a', 'f', 'n'] {
            assert_eq!(prompt_scroll(&key(KeyCode::Char(answer))), None);
        }
        assert_eq!(prompt_scroll(&key(KeyCode::Esc)), None);
    }

    #[test]
    fn kbv3d_modifier_chords_and_page_wheel_scroll_while_a_prompt_waits() {
        let chord = |code, modifiers| UserEvent::Key(KeyEvent::new(code, modifiers));
        let alt = KeyModifiers::ALT;
        let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
        for (event, expected) in [
            (chord(KeyCode::Up, alt), PromptScroll::PageUp),
            (chord(KeyCode::Down, alt), PromptScroll::PageDown),
            (chord(KeyCode::Up, alt_shift), PromptScroll::Top),
            (chord(KeyCode::Down, alt_shift), PromptScroll::Bottom),
            (chord(KeyCode::Char('<'), alt), PromptScroll::Top),
            (chord(KeyCode::Char('>'), alt_shift), PromptScroll::Bottom),
            (
                chord(KeyCode::Home, KeyModifiers::CONTROL),
                PromptScroll::Top,
            ),
            (
                chord(KeyCode::End, KeyModifiers::CONTROL),
                PromptScroll::Bottom,
            ),
            (UserEvent::ScrollPageUp, PromptScroll::PageUp),
            (UserEvent::ScrollPageDown, PromptScroll::PageDown),
        ] {
            assert_eq!(prompt_scroll(&event), Some(expected), "{event:?}");
        }
        // Plain arrows still move by line and answer keys are never claimed.
        assert_eq!(
            prompt_scroll(&chord(KeyCode::Up, KeyModifiers::NONE)),
            Some(PromptScroll::LineUp)
        );
        assert_eq!(prompt_scroll(&chord(KeyCode::Char('y'), alt)), None);
        assert_eq!(
            prompt_scroll(&chord(KeyCode::Char('<'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn stop_with_instruction_has_an_explicit_key() {
        assert_eq!(
            classify_prompt_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            PromptInput::StopWithInstruction
        );
        assert_eq!(
            classify_prompt_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            PromptInput::Ignore
        );
    }

    #[test]
    fn session_grants_are_labelled_as_session_scoped() {
        let plain = permission_options(None);
        assert!(plain.contains("(a) allow for this session"));
        assert!(!plain.contains("allow always"));
        assert!(!plain.contains("(f)"));

        let folder = std::path::Path::new("/home/me/.config/mini");
        let with_folder = permission_options(Some(folder));
        assert!(
            with_folder
                .contains("(f) allow read+edit+list in /home/me/.config/mini for this session")
        );
        assert!(with_folder.contains("(a) allow for this session"));
    }

    #[test]
    fn workspace_edit_answer_requires_plain_w() {
        let key = |c| classify_prompt_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        assert_eq!(key('w'), PromptInput::AllowWorkspaceEdits);
        assert_eq!(key('W'), PromptInput::AllowWorkspaceEdits);
        assert_eq!(
            classify_prompt_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            PromptInput::Ignore
        );
    }

    #[test]
    fn folder_and_session_keys_are_distinct_from_abort() {
        let key = |c| classify_prompt_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        assert_eq!(key('a'), PromptInput::AllowSession);
        assert_eq!(key('f'), PromptInput::AllowFolder);
        assert_eq!(key('n'), PromptInput::Deny);
        // mini-agent-wb29a: `e` expands the request, it never answers.
        assert_eq!(key('e'), PromptInput::ToggleFull);
        assert_eq!(key('E'), PromptInput::ToggleFull);
        assert_eq!(
            classify_prompt_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
            PromptInput::Ignore
        );
        assert_eq!(
            classify_prompt_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            PromptInput::Abort
        );
        assert_eq!(
            classify_prompt_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            PromptInput::Ignore
        );
    }

    #[test]
    fn prompt_grant_header_names_prompt_tools_and_folder() {
        let offer = PromptGrantOffer {
            prompt: "autoconfig".into(),
            dir: std::path::PathBuf::from("/cfg"),
            tools: vec!["read", "edit"],
        };
        assert_eq!(
            prompt_grant_header(&offer),
            "[permission] prompt 'autoconfig' asks for read, edit access to /cfg for this session"
        );
    }
}
