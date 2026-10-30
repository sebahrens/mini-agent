use crate::context;
use crate::session::storage;
use crate::ui::slash::{SlashCtx, write_error, write_ok, write_result};
use crate::ui::{PromptModeOutcome, apply_main_agent, apply_prompt_mode};

pub async fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match parts[0] {
        "/prompt" => handle_prompt(parts, ctx).await,
        "/agent" => handle_agent(parts, ctx).await,
        "/theme" => handle_theme(parts, ctx).await,
        "/regen-prompts" => handle_regen_prompts(ctx).await,
        "/regen-themes" => handle_regen_themes(ctx).await,
        _ => Ok(()),
    }
}

async fn handle_agent(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mut names = ctx
        .context
        .agent_definitions
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    names.sort_unstable();
    if parts.len() < 2 {
        let current = ctx
            .context
            .current_agent_name
            .as_deref()
            .unwrap_or("(default)");
        write_ok(
            ctx.renderer,
            format!("available main-agent personas (current: {current}):"),
        );
        for name in names {
            write_result(
                ctx.renderer,
                ctx.context.agent_definitions[&name].list_entry(&name),
            );
        }
        write_result(ctx.renderer, "usage: /agent <name>  |  /agent default");
        return Ok(());
    }

    let name = parts[1].trim();
    if name == "default" {
        ctx.context.current_agent_explicit = true;
        if ctx.context.current_agent_name.take().is_some() {
            ctx.rebuild_agent().await;
            write_ok(ctx.renderer, "main-agent persona cleared");
        } else {
            write_ok(ctx.renderer, "main-agent persona already uses the default");
        }
        return Ok(());
    }

    let Some(outcome) = apply_main_agent(name, ctx.context, ctx.permission) else {
        write_error(ctx.renderer, format!("unknown agent: '{name}'"));
        if !names.is_empty() {
            write_ok(ctx.renderer, "available main-agent personas:");
            for name in names {
                write_result(
                    ctx.renderer,
                    ctx.context.agent_definitions[&name].list_entry(&name),
                );
            }
        }
        return Ok(());
    };
    let source = ctx
        .context
        .agent_definitions
        .get(name)
        .map(|definition| definition.source_description(name))
        .unwrap_or_else(|| "unknown source".to_string());

    let mut rebuilt = false;
    if let Some(prompt_name) = outcome.default_prompt {
        if outcome.prompt_applied {
            rebuilt = ctx.switch_to_prompt_model(&prompt_name).await;
            write_ok(
                ctx.renderer,
                format!("active prompt: {prompt_name} (from agent '{name}')"),
            );
        } else {
            write_error(
                ctx.renderer,
                format!(
                    "agent '{name}' names unavailable prompt mode '{prompt_name}'; persona remains active"
                ),
            );
        }
    }
    let mode_from_agent = matches!(outcome.prompt_mode, PromptModeOutcome::Applied(_));
    match outcome.prompt_mode {
        PromptModeOutcome::RestoredUserMode => {
            if let Some(perm) = ctx.permission {
                let current = perm
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .mode();
                write_ok(ctx.renderer, format!("restored user mode: {current}"));
            }
        }
        PromptModeOutcome::Applied(mode) => {
            write_ok(
                ctx.renderer,
                format!("security mode: {mode} (from agent prompt)"),
            );
        }
        PromptModeOutcome::None => {}
    }
    if !rebuilt {
        ctx.rebuild_agent().await;
    }
    let mode = ctx.permission.as_ref().map(|perm| {
        perm.lock()
            .unwrap_or_else(|error| error.into_inner())
            .mode()
            .to_string()
    });
    write_ok(
        ctx.renderer,
        active_agent_message(name, &source, mode.as_deref(), mode_from_agent),
    );
    Ok(())
}

/// Confirmation for `/agent <name>` that states what a persona changes: its
/// prompt (and, when its prompt mode says so, the security mode) — never the
/// tool set, so a name like `rust-maintainer` is not mistaken for new powers.
fn active_agent_message(
    name: &str,
    source: &str,
    mode: Option<&str>,
    mode_from_agent: bool,
) -> String {
    let mode = mode.unwrap_or("unchanged");
    if mode_from_agent {
        format!(
            "active agent: {name} ({source}); persona prompt, security mode from its prompt (mode: {mode}); tools unchanged"
        )
    } else {
        format!(
            "active agent: {name} ({source}); persona prompt only; tools and security mode unchanged (mode: {mode})"
        )
    }
}

async fn handle_prompt(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mut sorted: Vec<&String> = ctx.context.prompts.keys().collect();
    sorted.sort();
    if parts.len() < 2 {
        if sorted.is_empty() {
            write_ok(ctx.renderer, "no prompts available");
        } else {
            let current = ctx
                .context
                .current_prompt_name
                .as_deref()
                .unwrap_or("(none)");
            write_ok(
                ctx.renderer,
                format!("available prompts (current: {}):", current),
            );
            for name in &sorted {
                write_result(ctx.renderer, format!("  {}", name));
            }
            write_result(ctx.renderer, "usage: /prompt <name>  |  /prompt default");
        }
    } else if parts[1] == "default" {
        if ctx.context.current_prompt.is_none() {
            write_ok(ctx.renderer, "no active prompt to clear");
        } else {
            ctx.context.current_prompt = None;
            ctx.context.current_prompt_name = None;
            // Revert to the config default model
            let default_model = ctx.cli.resolve_model(ctx.cfg);
            let default_provider = ctx.cli.resolve_provider(ctx.cfg);
            let provider_changed = default_provider != ctx.session.provider;
            match provider_changed.then(|| ctx.switch_client(&default_provider)) {
                Some(Err(e)) => {
                    // Keep the current provider with its own model.
                    write_error(ctx.renderer, format!("failed to revert provider: {}", e));
                }
                Some(Ok(())) | None => ctx.session.model = default_model,
            }
            ctx.rebuild_agent().await;
            write_ok(ctx.renderer, "prompt cleared (back to default)");
        }
    } else {
        let name = parts[1].trim();
        if ctx.context.prompts.contains_key(name) {
            write_prompt_warnings(ctx.renderer, &ctx.context.prompts[name]);
            match apply_prompt_mode(name, ctx.context, ctx.permission) {
                PromptModeOutcome::RestoredUserMode => {
                    if let Some(perm) = ctx.permission {
                        let current = perm
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .mode()
                            .to_string();
                        write_ok(ctx.renderer, format!("restored user mode: {}", current));
                    }
                }
                PromptModeOutcome::Applied(mode) => {
                    write_ok(
                        ctx.renderer,
                        format!("security mode: {} (from prompt)", mode),
                    );
                }
                PromptModeOutcome::None => {}
            }
            let model_switched = ctx.switch_to_prompt_model(name).await;
            if !model_switched {
                ctx.rebuild_agent().await;
            }
            write_ok(ctx.renderer, format!("active prompt: {}", name));
        } else {
            write_error(ctx.renderer, format!("unknown prompt: '{}'", name));
            if !sorted.is_empty() {
                write_ok(ctx.renderer, "available prompts:");
                for p in &sorted {
                    write_result(ctx.renderer, format!("  {}", p));
                }
            }
        }
    }
    Ok(())
}

fn write_prompt_warnings(renderer: &mut crate::ui::renderer::Renderer, content: &str) {
    for warning in context::prompts::parse_directives(content).warnings {
        write_result(renderer, format!("prompt warning: {warning}"));
    }
}

async fn handle_theme(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let mut sorted: Vec<&String> = ctx.context.themes.keys().collect();
    sorted.sort();
    if parts.len() < 2 {
        if sorted.is_empty() {
            write_ok(ctx.renderer, "no themes available");
        } else {
            let current = ctx
                .context
                .current_theme_name
                .as_deref()
                .unwrap_or("(none)");
            write_ok(
                ctx.renderer,
                format!("available themes (current: {}):", current),
            );
            for name in &sorted {
                write_result(ctx.renderer, format!("  {}", name));
            }
            write_result(ctx.renderer, "usage: /theme <name>  |  /theme default");
        }
    } else if parts[1] == "default" {
        if ctx.context.current_theme_name.is_none() && storage::load_theme_name().is_none() {
            write_ok(ctx.renderer, "no active theme to clear");
        } else {
            ctx.context.current_theme_name = None;
            if let Err(e) = storage::save_theme_name(None) {
                write_error(
                    ctx.renderer,
                    format!("warning: theme choice not saved: {}", e),
                );
            }
            let defaults = crate::config::ColorsConfig::default();
            let colors = ctx.cfg.colors.as_ref().unwrap_or(&defaults);
            crate::context::themes::apply_colors(colors, ctx.renderer, ctx.input);
            write_ok(ctx.renderer, "theme cleared (using config colors)");
        }
    } else {
        let name = parts[1].trim();
        if let Some(content) = ctx.context.themes.get(name) {
            if let Err(error) = crate::context::themes::apply(content, ctx.renderer, ctx.input) {
                write_error(
                    ctx.renderer,
                    format!("could not apply theme '{}': {}", name, error),
                );
                return Ok(());
            }
            ctx.context.current_theme_name = Some(name.to_string());
            if let Err(e) = storage::save_theme_name(Some(name)) {
                write_error(
                    ctx.renderer,
                    format!("warning: theme choice not saved: {}", e),
                );
            }
            write_ok(ctx.renderer, format!("active theme: {}", name));
        } else {
            write_error(ctx.renderer, format!("unknown theme: '{}'", name));
            if !sorted.is_empty() {
                write_ok(ctx.renderer, "available themes:");
                for t in &sorted {
                    write_result(ctx.renderer, format!("  {}", t));
                }
            }
        }
    }
    Ok(())
}

async fn handle_regen_prompts(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match context::prompts::regen() {
        Ok(report) => {
            ctx.context.prompts = context::prompts::load();
            write_ok(
                ctx.renderer,
                format!(
                    "default prompts regenerated{}",
                    crate::startup::backup_note(&report)
                ),
            );
        }
        Err(e) => {
            write_error(ctx.renderer, format!("failed to regenerate prompts: {}", e));
        }
    }
    Ok(())
}

async fn handle_regen_themes(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match context::themes::regen() {
        Ok(report) => {
            ctx.context.themes = context::themes::load();
            write_ok(
                ctx.renderer,
                format!(
                    "default themes regenerated{}",
                    crate::startup::backup_note(&report)
                ),
            );
        }
        Err(e) => {
            write_error(ctx.renderer, format!("failed to regenerate themes: {}", e));
        }
    }
    Ok(())
}

#[cfg(test)]
mod active_agent_message_tests {
    use super::{active_agent_message, write_prompt_warnings};

    #[test]
    fn prompt_mode_warnings_reach_activation_output() {
        let mut renderer = crate::ui::renderer::Renderer::new().unwrap();
        write_prompt_warnings(
            &mut renderer,
            "%%mode=\n%%unknown=value\n%%mode=readonly\n%%mode=yolo\nBody.",
        );
        let feed = renderer.feed();
        let lines: Vec<_> = (0..feed.block_count())
            .map(|index| feed.block_text(index).unwrap().to_string())
            .collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("prompt warning: ignoring empty prompt directive %%mode="));
        assert!(lines[1].contains("prompt warning: ignoring unknown prompt directive %%unknown="));
        assert!(lines[2].contains(
            "prompt warning: ignoring duplicate prompt directive %%mode=; first nonempty value wins"
        ));
    }

    #[test]
    fn persona_confirmation_states_capabilities_are_unchanged() {
        assert_eq!(
            active_agent_message("rust-maintainer", "global", Some("standard"), false),
            "active agent: rust-maintainer (global); persona prompt only; tools and security mode unchanged (mode: standard)"
        );
        let applied = active_agent_message("reviewer", "project", Some("plan"), true);
        assert!(applied.contains("security mode from its prompt (mode: plan)"));
        assert!(applied.ends_with("tools unchanged"));
    }
}

#[cfg(test)]
mod theme_selection_tests {
    use super::*;
    use crossterm::style::Color;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn theme_selection_reports_parse_error_and_preserves_active_and_saved_choice() {
        let root = crate::tests::OwnedTestDirectory::new("theme-selection-errors");
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
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli::default();
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let directory = context::themes::global_dir();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("good.json"),
            r##"{"chat_background":"#ff0000"}"##,
        )
        .unwrap();
        std::fs::write(
            directory.join("broken.json"),
            "{\n  \"chat_background\":\n}",
        )
        .unwrap();
        let mut context = context::load(true).for_workspace_binding(true, &workspace);
        let mut renderer = crate::ui::renderer::Renderer::new().unwrap();
        let mut session = crate::session::Session::new("anthropic", "fixture", 128_000, "");
        let mut client = crate::provider::create_client(
            "anthropic",
            Some("unused-test-key"),
            cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap();
        let mut agent = None;
        let mut input = crate::ui::input::InputEditor::new();
        let invalidated = AtomicBool::new(false);
        let mut show_reasoning = false;
        let mut reasoning_enabled = false;
        let mut is_running = false;
        let mut todo_tools_enabled = false;
        let mut sandbox = crate::sandbox::Sandbox::new(false, "none");
        let mut terminal_guard = crate::ui::terminal::TerminalGuard::detached_for_test();
        #[cfg(feature = "skills")]
        let skill_services = Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new());
        let mut ctx = SlashCtx {
            prebuild_invalidated: &invalidated,
            agent: &mut agent,
            client: &mut client,
            renderer: &mut renderer,
            session: &mut session,
            cli: &cli,
            cfg: &cfg,
            context: &mut context,
            workspace: &workspace,
            custom_commands: &mut Default::default(),
            show_reasoning: &mut show_reasoning,
            reasoning_enabled: &mut reasoning_enabled,
            is_running: &mut is_running,
            input: &mut input,
            permission: &None,
            ask_tx: &None,
            todo_tools_enabled: &mut todo_tools_enabled,
            sandbox: &mut sandbox,
            terminal_guard: &mut terminal_guard,
            #[cfg(feature = "skills")]
            skill_services: &skill_services,
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        handle(&["/theme", "good"], &mut ctx).await.unwrap();
        assert_eq!(ctx.context.current_theme_name.as_deref(), Some("good"));
        assert_eq!(storage::load_theme_name().as_deref(), Some("good"));
        let prior_colors = ctx.renderer.background_colors_for_test();
        assert_eq!(
            prior_colors,
            (Some(Color::Rgb { r: 255, g: 0, b: 0 }), None, None)
        );
        handle(&["/theme", "broken"], &mut ctx).await.unwrap();
        assert_eq!(ctx.context.current_theme_name.as_deref(), Some("good"));
        assert_eq!(storage::load_theme_name().as_deref(), Some("good"));
        assert_eq!(ctx.renderer.background_colors_for_test(), prior_colors);
        let feed = ctx.renderer.feed();
        let transcript: Vec<_> = (0..feed.block_count())
            .filter_map(|index| feed.block_text(index))
            .collect();
        assert!(
            transcript
                .iter()
                .any(|line| line.contains("could not apply theme 'broken'")
                    && line.contains("line 3 column 1")),
            "{transcript:?}"
        );
        assert!(
            !transcript
                .iter()
                .any(|line| line.contains("active theme: broken")),
            "{transcript:?}"
        );
        ctx.context.themes.insert("broken".into(), "{}".into());
        handle(&["/theme", "broken"], &mut ctx).await.unwrap();
        assert_eq!(ctx.context.current_theme_name.as_deref(), Some("broken"));
        assert_eq!(storage::load_theme_name().as_deref(), Some("broken"));
        assert_eq!(
            ctx.renderer.background_colors_for_test(),
            (None, None, None)
        );
        ctx.context.themes.insert("broken".into(), "{".into());
        assert!(
            context::themes::apply_initial(
                ctx.context,
                ctx.cfg.colors.as_ref(),
                ctx.renderer,
                ctx.input,
            )
            .is_some()
        );
        assert_eq!(ctx.context.current_theme_name, None);
        assert_eq!(storage::load_theme_name().as_deref(), Some("broken"));
        handle(&["/theme", "default"], &mut ctx).await.unwrap();
        assert_eq!(
            storage::load_theme_name(),
            None,
            "default can clear a rejected saved choice"
        );
    }
}
