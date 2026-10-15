//! Operator settings editor. Persisted settings do not mutate active authority.
use super::{SlashCtx, write_error, write_ok};
use crate::config::editor::{self, Target};

pub(crate) enum Command<'a> {
    Browse(Target, Option<&'static str>),
    Set(Target, &'static str, &'a str),
}
pub(crate) fn parse(text: &str) -> Result<Command<'_>, &'static str> {
    if text.len() > 32 * 1024 {
        return Err("Settings command is too large (values omitted)");
    }
    let text = text.trim();
    let rest = text
        .strip_prefix("/config")
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        .ok_or("usage: /config [--global|--project|--local] [key [TOML value]]")?
        .trim_start();
    let (target, rest) = if let Some(rest) = rest
        .strip_prefix("--global")
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        (Target::Global, rest.trim_start())
    } else if let Some(rest) = rest
        .strip_prefix("--project")
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        (Target::Project, rest.trim_start())
    } else if let Some(rest) = rest
        .strip_prefix("--local")
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        (Target::Local, rest.trim_start())
    } else {
        (Target::Global, rest)
    };
    if rest.is_empty() {
        return Ok(Command::Browse(target, None));
    }
    let (name, value) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let key = crate::config::keys::lookup(name)
        .filter(|key| key.feature.is_none_or(crate::config::keys::feature_enabled))
        .ok_or("Unknown or unavailable setting (values omitted)")?;
    let value = value.trim();
    if value.is_empty() {
        Ok(Command::Browse(target, Some(key.name)))
    } else {
        Ok(Command::Set(target, key.name, value))
    }
}
pub(crate) fn is_command(text: &str) -> bool {
    text.split_whitespace().next() == Some("/config")
}
pub(crate) fn is_browse(text: &str) -> bool {
    matches!(parse(text), Ok(Command::Browse(..)))
}
pub(crate) fn needs_tty(text: &str) -> bool {
    matches!(
        parse(text),
        Ok(Command::Set(Target::Project | Target::Local, ..))
    )
}
async fn owned<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, &'static str> {
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let cancellation = scope.cancellation_handle();
    let result = scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                if cancellation.is_cancelled() {
                    Err(editor::ERROR)
                } else {
                    Ok(operation())
                }
            })
            .await
        })
        .await;
    cleanup.settle().await;
    result.map_err(|_| editor::ERROR)?
}
fn confirm(description: &str) -> std::io::Result<bool> {
    use std::io::{BufRead, Read, Write};
    println!("{description}\nApprove exact candidate? [y/N]");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().take(32).read_line(&mut answer)?;
    Ok(answer.ends_with('\n') && matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}
pub(crate) async fn handle(text: &str, ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let command = match parse(text) {
        Ok(command) => command,
        Err(error) => {
            write_error(ctx.renderer, error);
            return Ok(());
        }
    };
    let (target, key, literal) = match command {
        Command::Browse(target, key) => {
            ctx.input.open_config_picker(ctx.cfg, ctx.cli, target, key);
            #[cfg(feature = "subagents")]
            let scope = "startup settings and captured next-delegation subagent limits; saved subagent sizing applies to the next delegation, other saved policy requires restart";
            #[cfg(not(feature = "subagents"))]
            let scope = "startup-effective configuration; saved policy requires restart";
            write_ok(
                ctx.renderer,
                format!(
                    "settings: {} target; values and sources describe {scope}. Choose a key; enter a TOML value.",
                    target.label()
                ),
            );
            return Ok(());
        }
        Command::Set(target, key, literal) => (target, key, literal),
    };
    if *ctx.is_running {
        write_error(
            ctx.renderer,
            "Finish or stop the current turn before saving settings",
        );
        return Ok(());
    }
    let paths = match crate::paths::process_paths() {
        Ok(paths) => paths,
        Err(_) => {
            write_error(ctx.renderer, editor::ERROR);
            return Ok(());
        }
    };
    let global = ctx
        .cfg
        .provenance
        .global_file
        .clone()
        .unwrap_or_else(|| crate::config::pick_existing(&paths.config_dir));
    let workspace = ctx.workspace.clone();
    let home = crate::paths::process_home_dir();
    let literal_owned = literal.to_owned();
    let paths_owned = paths.clone();
    let prepared = owned(move || {
        editor::prepare(
            &paths_owned,
            &global,
            workspace,
            home.as_deref(),
            target,
            key,
            &literal_owned,
        )
    })
    .await;
    let edit = match prepared {
        Ok(Ok(edit)) => edit,
        Ok(Err(error)) | Err(error) => {
            write_error(ctx.renderer, error);
            return Ok(());
        }
    };
    let approved = if let Some(description) = &edit.confirmation {
        ctx.terminal_guard.suspend()?;
        let decision = confirm(description);
        ctx.terminal_guard.resume()?;
        match decision {
            Ok(decision) => decision,
            Err(_) => {
                write_error(
                    ctx.renderer,
                    "Approval could not be read; settings were not changed",
                );
                return Ok(());
            }
        }
    } else {
        true
    };
    let result = owned(move || edit.publish(approved)).await;
    match result {
        Ok(Ok(())) => {
            // Presentation and finite subagent sizing can update in place.
            // A higher captured layer still wins; no provider/tool/permission is rebuilt.
            let higher = ctx.cfg.provenance.sources.get(key).is_some_and(|sources| {
                sources.iter().any(|source| match target {
                    Target::Global => !matches!(source, crate::config::ConfigLayer::Global),
                    Target::Project => matches!(
                        source,
                        crate::config::ConfigLayer::Local | crate::config::ConfigLayer::Cli
                    ),
                    Target::Local => matches!(source, crate::config::ConfigLayer::Cli),
                })
            });
            #[cfg(feature = "subagents")]
            if !higher && crate::extras::subagents::LIVE_SETTING_KEYS.contains(&key) {
                let layer = match target {
                    Target::Global => crate::config::ConfigLayer::Global,
                    Target::Project => crate::config::ConfigLayer::Project,
                    Target::Local => crate::config::ConfigLayer::Local,
                };
                match crate::extras::subagents::apply_saved_limit(key, literal, layer) {
                    Ok(true) => {
                        write_ok(
                            ctx.renderer,
                            "Saved; applies to the next delegation. Running children retain their captured limits.",
                        );
                        return Ok(());
                    }
                    Ok(false) => {}
                    Err(error) => {
                        write_error(ctx.renderer, format!("Saved; restart to apply: {error}"));
                        return Ok(());
                    }
                }
            }
            let live = if key == "reasoning_display" && !higher {
                match literal {
                    "\"off\"" => Some(crate::config::ReasoningDisplay::Off),
                    "\"collapsed\"" => Some(crate::config::ReasoningDisplay::Collapsed),
                    "\"full\"" => Some(crate::config::ReasoningDisplay::Full),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(display) = live {
                *ctx.show_reasoning = display != crate::config::ReasoningDisplay::Off;
                ctx.renderer.feed_mut().set_reasoning_display(display);
            }
            write_ok(
                ctx.renderer,
                if live.is_some() {
                    "Saved; reasoning display updated. Other startup values and higher CLI/layer overrides remain active."
                } else {
                    "Saved. Restart to apply; higher local/CLI overrides still take precedence."
                },
            );
        }
        Ok(Err(error)) | Err(error) => write_error(ctx.renderer, error),
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config68_classifier_keeps_busy_writes_and_literal_values_distinct() {
        assert!(is_browse("/config --local"));
        assert!(is_browse("/config model"));
        assert!(!is_browse("/config model \"x y\""));
        assert!(!is_command("/configuration"));
        assert!(needs_tty("/config --project sandbox false"));
        assert!(!needs_tty("/config --project"));
        assert!(matches!(
            parse("/config --local model \"x y\""),
            Ok(Command::Set(Target::Local, "model", "\"x y\""))
        ));
        assert!(parse("/config api_keys x-secret").is_ok());
        assert!(
            !parse("/config unknown-secret 9")
                .err()
                .unwrap()
                .contains("unknown-secret")
        );
    }
}
