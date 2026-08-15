use crossterm::style::Color;
use tokio::sync::mpsc;

use crate::event::UserEvent;
use crate::ui::renderer::Renderer;
use crate::ui::state::{AgentRunState, UiContext};
use crate::ui::utils::suggest_pattern;

use super::{C_ERROR, C_PERM};

pub async fn handle_permission_request(
    ask_req: crate::permission::ask::AskRequest,
    renderer: &mut Renderer,
    ui: &mut UiContext<'_>,
    run: &mut AgentRunState,
    user_rx: &mut mpsc::Receiver<UserEvent>,
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
            Some(ev) = user_rx.recv() => {
                if let crate::event::UserEvent::Key(key) = ev {
                    match key.code {
                        crossterm::event::KeyCode::Char('y') => break crate::permission::ask::UserDecision::AllowOnce,
                        crossterm::event::KeyCode::Char('a') => {
                            let pattern = ask_req.suggested_pattern.clone().unwrap_or_else(|| {
                                suggest_pattern(&ask_req.tool, &ask_req.input)
                            });
                            renderer.write_line(
                                &format!("  -> will allow: {}", pattern),
                                Color::Green,
                            )?;
                            break crate::permission::ask::UserDecision::AllowAlways(pattern);
                        }
                        crossterm::event::KeyCode::Char('n') | crossterm::event::KeyCode::Esc => break crate::permission::ask::UserDecision::Deny,
                        _ => {}
                    }
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
