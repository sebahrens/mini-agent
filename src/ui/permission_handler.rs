use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Color;
use tokio::sync::mpsc;

use crate::event::UserEvent;
use crate::ui::renderer::Renderer;
use crate::ui::state::{AgentRunState, UiContext};
use crate::ui::utils::suggest_pattern;

use super::{C_ERROR, C_PERM};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptInput {
    AllowOnce,
    AllowAlways,
    Deny,
    Ignore,
}

fn classify_prompt_key(key: KeyEvent) -> PromptInput {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C' | 'd' | 'D'))
    {
        return PromptInput::Deny;
    }
    let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Char('y' | 'Y') if plain => PromptInput::AllowOnce,
        KeyCode::Char('a' | 'A') if plain => PromptInput::AllowAlways,
        KeyCode::Char('n' | 'N') if plain => PromptInput::Deny,
        KeyCode::Esc => PromptInput::Deny,
        _ => PromptInput::Ignore,
    }
}

fn defer_during_prompt(event: &UserEvent) -> bool {
    matches!(event, UserEvent::Resize | UserEvent::Paste(_))
}

pub async fn handle_permission_request(
    ask_req: crate::permission::ask::AskRequest,
    renderer: &mut Renderer,
    ui: &mut UiContext<'_>,
    run: &mut AgentRunState,
    user_rx: &mut mpsc::Receiver<UserEvent>,
    deferred_user_events: &mut VecDeque<UserEvent>,
) -> anyhow::Result<()> {
    run.was_reasoning = false;
    if run.agent_line_started {
        renderer.write_line("", Color::White)?;
        run.agent_line_started = false;
    }

    renderer.write_line(
        &format!("[permission] {}: {}", ask_req.tool, ask_req.input),
        C_PERM,
    )?;
    renderer.write_line(
        "  (y) allow once  (a) allow always  (n) deny  (ESC) abort",
        C_PERM,
    )?;

    renderer.permission_prompt = Some(super::renderer::PermissionPrompt {
        tool: format!("[permission] {}: {}", ask_req.tool, ask_req.input).into(),
        options: "  (y) allow once  (a) allow always  (n) deny  (ESC) abort".into(),
    });
    renderer.render_viewport()?;
    renderer.draw_bottom("", 0, &[], 0, false)?;

    let decision = loop {
        tokio::select! {
            ev = user_rx.recv() => {
                match ev {
                    Some(UserEvent::Key(key)) => match classify_prompt_key(key) {
                        PromptInput::AllowOnce => break crate::permission::ask::UserDecision::AllowOnce,
                        PromptInput::AllowAlways => {
                            let pattern = ask_req.suggested_pattern.clone().unwrap_or_else(|| {
                                suggest_pattern(&ask_req.tool, &ask_req.input)
                            });
                            renderer.write_line(
                                &format!("  -> will allow: {}", pattern),
                                Color::Green,
                            )?;
                            break crate::permission::ask::UserDecision::AllowAlways(pattern);
                        }
                        PromptInput::Deny => break crate::permission::ask::UserDecision::Deny,
                        PromptInput::Ignore => {}
                    },
                    Some(event) if defer_during_prompt(&event) => {
                        deferred_user_events.push_back(event);
                    }
                    Some(_) => {}
                    None => break crate::permission::ask::UserDecision::Deny,
                }
            }
        }
    };

    renderer.permission_prompt = None;

    let allow_patterns = match &decision {
        crate::permission::ask::UserDecision::AllowAlways(p) => {
            let mut patterns = vec![p.clone()];
            patterns.extend(ask_req.additional_allow_patterns.iter().cloned());
            patterns
        }
        _ => Vec::new(),
    };
    let _ = ask_req.reply.send(decision);

    for pattern in allow_patterns {
        renderer.write_line(
            &format!("  allowed {} {} for this session", ask_req.tool, pattern),
            Color::Green,
        )?;
        ui.session
            .permission_allowlist
            .push(crate::session::PermissionAllowEntry {
                tool: ask_req.tool.clone(),
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
                PromptInput::Deny
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
    fn resize_and_paste_are_deferred_until_after_the_prompt() {
        assert!(defer_during_prompt(&UserEvent::Resize));
        assert!(defer_during_prompt(&UserEvent::Paste("text".into())));
        assert!(!defer_during_prompt(&UserEvent::ScrollUp));
    }
}
