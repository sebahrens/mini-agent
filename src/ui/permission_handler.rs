use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Color;
use tokio::sync::mpsc;

use crate::event::UserEvent;
use crate::permission::checker::PromptGrantOffer;
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
    Deny,
    /// Esc / Ctrl+C / Ctrl+D: refuse without further questions.
    Abort,
    Ignore,
}

/// Options line for an ordinary approval. Session-scoped grants are labelled
/// as such: nothing here persists beyond the session.
fn permission_options(folder: Option<&std::path::Path>) -> String {
    match folder {
        Some(folder) => format!(
            "  (y) allow once  (a) allow for this session  (f) allow read+edit+list in {} for this session  (n) deny  (ESC) abort",
            folder.display()
        ),
        None => "  (y) allow once  (a) allow for this session  (n) deny  (ESC) abort".to_string(),
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

const PROMPT_GRANT_OPTIONS: &str =
    "  (y) allow for this session  (n) ask for each access instead  (ESC) deny this request";

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
        KeyCode::Char('n' | 'N') if plain => PromptInput::Deny,
        KeyCode::Esc => PromptInput::Abort,
        _ => PromptInput::Ignore,
    }
}

fn defer_during_prompt(event: &UserEvent) -> bool {
    matches!(event, UserEvent::Resize | UserEvent::Paste(_))
}

/// Show `header` and `options`, then wait for one recognised key. Resize and
/// paste events are deferred; a closed input channel aborts.
async fn read_prompt_input(
    header: &str,
    options: &str,
    renderer: &mut Renderer,
    user_rx: &mut mpsc::Receiver<UserEvent>,
    deferred_user_events: &mut VecDeque<UserEvent>,
    accept: impl Fn(PromptInput) -> bool,
) -> anyhow::Result<PromptInput> {
    renderer.write_line(header, C_PERM)?;
    renderer.write_line(options, C_PERM)?;
    renderer.permission_prompt = Some(super::renderer::PermissionPrompt {
        tool: header.into(),
        options: options.into(),
    });
    renderer.render_viewport()?;
    renderer.draw_bottom("", 0, &[], 0, false)?;

    let input = loop {
        match user_rx.recv().await {
            Some(UserEvent::Key(key)) => {
                let input = classify_prompt_key(key);
                if input != PromptInput::Ignore && accept(input) {
                    break input;
                }
            }
            Some(event) if defer_during_prompt(&event) => {
                deferred_user_events.push_back(event);
            }
            Some(_) => {}
            None => break PromptInput::Abort,
        }
    };
    renderer.permission_prompt = None;
    Ok(input)
}

pub async fn handle_permission_request(
    ask_req: crate::permission::ask::AskRequest,
    renderer: &mut Renderer,
    ui: &mut UiContext<'_>,
    run: &mut AgentRunState,
    user_rx: &mut mpsc::Receiver<UserEvent>,
    deferred_user_events: &mut VecDeque<UserEvent>,
) -> anyhow::Result<()> {
    use crate::permission::ask::UserDecision;

    run.was_reasoning = false;
    if run.agent_line_started {
        renderer.write_line("", Color::White)?;
        run.agent_line_started = false;
    }

    let (prompt_offer, folder) = match ui.permission.as_ref() {
        Some(perm) => {
            let guard = perm.lock().unwrap_or_else(|e| e.into_inner());
            (
                guard.prompt_grant_offer_for(&ask_req.tool, &ask_req.input),
                guard.folder_grant_scope(&ask_req.tool, &ask_req.input),
            )
        }
        None => (None, None),
    };

    // A built-in prompt's scoped grant is offered once, before the first
    // per-call approval it would cover. Declining falls back to per-call.
    if let Some(offer) = prompt_offer {
        let header = prompt_grant_header(&offer);
        let input = read_prompt_input(
            &header,
            PROMPT_GRANT_OPTIONS,
            renderer,
            user_rx,
            deferred_user_events,
            |input| {
                matches!(
                    input,
                    PromptInput::AllowOnce | PromptInput::Deny | PromptInput::Abort
                )
            },
        )
        .await?;
        let Some(perm) = ui.permission.clone() else {
            let _ = ask_req.reply.send(UserDecision::Deny);
            return Ok(());
        };
        match input {
            PromptInput::AllowOnce => {
                let entries = perm
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .accept_prompt_grant();
                let _ = ask_req.reply.send(UserDecision::AllowOnce);
                return record_session_grants(entries, renderer, ui, run);
            }
            PromptInput::Abort => {
                perm.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .decline_prompt_grant();
                let _ = ask_req.reply.send(UserDecision::Deny);
                return Ok(());
            }
            _ => {
                perm.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .decline_prompt_grant();
            }
        }
    }

    let header = format!("[permission] {}: {}", ask_req.tool, ask_req.input);
    let options = permission_options(folder.as_deref());
    let has_folder = folder.is_some();
    let input = read_prompt_input(
        &header,
        &options,
        renderer,
        user_rx,
        deferred_user_events,
        |input| input != PromptInput::AllowFolder || has_folder,
    )
    .await?;

    let (decision, entries) = match input {
        PromptInput::AllowOnce => (UserDecision::AllowOnce, Vec::new()),
        PromptInput::AllowSession => {
            let pattern = ask_req
                .suggested_pattern
                .clone()
                .unwrap_or_else(|| suggest_pattern(&ask_req.tool, &ask_req.input));
            renderer.write_line(&format!("  -> will allow: {}", pattern), Color::Green)?;
            let mut entries = vec![(ask_req.tool.to_string(), pattern.clone())];
            entries.extend(
                ask_req
                    .additional_allow_patterns
                    .iter()
                    .map(|extra| (ask_req.tool.to_string(), extra.clone())),
            );
            (UserDecision::AllowAlways(pattern), entries)
        }
        PromptInput::AllowFolder => match (&folder, ui.permission.as_ref()) {
            (Some(folder), Some(perm)) => {
                let entries = perm
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .add_session_folder_grant(
                        folder,
                        &crate::permission::checker::FOLDER_GRANT_TOOLS,
                    );
                renderer.write_line(
                    &format!(
                        "  -> will allow read, edit and list_dir in {}",
                        folder.display()
                    ),
                    Color::Green,
                )?;
                (UserDecision::AllowOnce, entries)
            }
            _ => (UserDecision::Deny, Vec::new()),
        },
        PromptInput::Deny | PromptInput::Abort | PromptInput::Ignore => {
            (UserDecision::Deny, Vec::new())
        }
    };
    let _ = ask_req.reply.send(decision);
    record_session_grants(entries, renderer, ui, run)
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
    fn resize_and_paste_are_deferred_until_after_the_prompt() {
        assert!(defer_during_prompt(&UserEvent::Resize));
        assert!(defer_during_prompt(&UserEvent::Paste("text".into())));
        assert!(!defer_during_prompt(&UserEvent::ScrollUp));
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
    fn folder_and_session_keys_are_distinct_from_abort() {
        let key = |c| classify_prompt_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        assert_eq!(key('a'), PromptInput::AllowSession);
        assert_eq!(key('f'), PromptInput::AllowFolder);
        assert_eq!(key('n'), PromptInput::Deny);
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
