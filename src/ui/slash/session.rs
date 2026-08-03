use std::io::Read;

use compact_str::CompactString;

use crate::ui::events::render_session;
use crate::ui::slash::{SlashCtx, undo_last, write_error, write_ok, write_result};

fn format_session_line(s: &crate::session::Session) -> String {
    let last = s
        .messages
        .last()
        .map(|m| format!("...{}", m.content.chars().take(30).collect::<String>()))
        .unwrap_or_default();
    let time = crate::ui::events::format_time(&s.updated_at);
    let name_part = if s.name.is_empty() {
        String::new()
    } else {
        format!("  [{}]", s.name)
    };
    format!(
        "  {}  {}  {}msgs  {}  {}{}",
        &s.id[..8],
        time,
        s.messages.len(),
        s.model,
        last,
        name_part
    )
}

pub async fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match parts[0] {
        "/sessions" => handle_sessions(parts, ctx).await,
        "/rename" => handle_rename(parts, ctx).await,
        "/clear" | "/new" => handle_clear(ctx).await,
        "/undo" => handle_undo(ctx).await,
        "/redo" => handle_redo(ctx).await,
        "/rewind" => handle_rewind(ctx).await,
        "/retry" => handle_retry(ctx).await,
        "/quit" | "/exit" => handle_quit(ctx).await,
        "/history" => handle_history(ctx).await,
        #[cfg(feature = "export")]
        "/export" => handle_export(parts, ctx).await,
        #[cfg(feature = "export")]
        "/import" => handle_import(parts, ctx).await,
        #[cfg(feature = "export")]
        "/share" => handle_share(ctx).await,
        _ => Ok(()),
    }
}

#[cfg(feature = "export")]
async fn handle_export(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let default_name = format!(
        "zerostack-session-{}.html",
        &ctx.session.id[..8.min(ctx.session.id.len())]
    );
    let path = parts
        .get(1)
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .unwrap_or(&default_name);
    let (content, kind) = if path.ends_with(".jsonl") {
        let content = match crate::extras::export::session_to_jsonl(ctx.session) {
            Ok(content) => content,
            Err(error) => {
                write_error(ctx.renderer, format!("export failed: {}", error));
                return Ok(());
            }
        };
        (content, "JSONL")
    } else {
        (crate::extras::export::session_to_html(ctx.session), "HTML")
    };
    match std::fs::write(path, content) {
        Ok(()) => write_ok(ctx.renderer, format!("exported {} to {}", kind, path)),
        Err(e) => write_error(ctx.renderer, format!("export failed: {}", e)),
    }
    Ok(())
}

#[cfg(feature = "export")]
async fn handle_import(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let Some(path) = parts.get(1).map(|p| p.trim()).filter(|p| !p.is_empty()) else {
        write_error(ctx.renderer, "usage: /import <file.jsonl|session.json>");
        return Ok(());
    };
    let content = match read_bounded_import(path) {
        Ok(c) => c,
        Err(e) => {
            write_error(ctx.renderer, format!("failed to read {}: {}", path, e));
            return Ok(());
        }
    };

    let mut session = match parse_imported_session(&content, ctx.session, ctx.cfg) {
        Ok(session) => session,
        Err(error) => {
            write_error(ctx.renderer, format!("invalid session file: {}", error));
            return Ok(());
        }
    };

    if session.name.is_empty() {
        session.name = CompactString::new("imported");
    }
    session.overhead_tokens =
        crate::agent::builder::estimate_overhead(ctx.context, *ctx.reasoning_enabled);
    let new_client = match crate::provider::create_client(
        &session.provider,
        ctx.cli.api_key.as_deref(),
        &ctx.cfg.custom_providers_map(),
        ctx.cfg.api_keys.as_ref(),
    ) {
        Ok(client) => client,
        Err(error) => {
            write_error(
                ctx.renderer,
                format!("cannot activate imported provider: {}", error),
            );
            return Ok(());
        }
    };
    let new_agent = ctx
        .build_agent_for_client(&new_client, &session.model)
        .await;
    let msg_count = session.messages.len();
    if let Err(e) = commit_staged_import(
        ctx.session,
        ctx.client,
        ctx.agent,
        session,
        new_client,
        new_agent,
        crate::session::storage::save_session,
    ) {
        write_error(ctx.renderer, format!("failed to save session: {}", e));
        return Ok(());
    }
    #[cfg(feature = "advisor")]
    {
        crate::extras::advisor::update_client(ctx.client.clone());
        crate::extras::advisor::set_session_messages(ctx.session.messages.clone());
    }
    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
    write_ok(
        ctx.renderer,
        format!("imported session from {} ({} msgs)", path, msg_count),
    );
    Ok(())
}

#[cfg(feature = "export")]
fn commit_staged_import<S, C, A>(
    current_session: &mut S,
    current_client: &mut C,
    current_agent: &mut Option<A>,
    new_session: S,
    new_client: C,
    new_agent: A,
    persist: impl FnOnce(&S) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    persist(&new_session)?;
    *current_client = new_client;
    *current_agent = Some(new_agent);
    *current_session = new_session;
    Ok(())
}

#[cfg(feature = "export")]
fn read_bounded_import(path: &str) -> anyhow::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take((crate::extras::export::MAX_SESSION_IMPORT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > crate::extras::export::MAX_SESSION_IMPORT_BYTES {
        anyhow::bail!(
            "session import exceeds the {} byte limit",
            crate::extras::export::MAX_SESSION_IMPORT_BYTES
        );
    }
    String::from_utf8(bytes).map_err(|error| anyhow::anyhow!("session file is not UTF-8: {error}"))
}

#[cfg(feature = "export")]
fn parse_imported_session(
    content: &str,
    current: &crate::session::Session,
    cfg: &crate::config::Config,
) -> anyhow::Result<crate::session::Session> {
    match crate::extras::export::parse_session_file(content)? {
        crate::extras::export::ParsedSessionFile::Native(session) => Ok(session),
        crate::extras::export::ParsedSessionFile::Jsonl(import) => {
            crate::paths::validate_portable_component(&import.id)?;
            let mut session = crate::session::Session::new(
                import.provider.as_str(),
                import.model.as_str(),
                current.context_window,
                import.name.as_str(),
            );
            session.id = import.id;
            session.created_at = import.created_at;
            session.working_dir = current.working_dir.clone();
            session.total_estimated_tokens =
                import.messages.iter().fold(0_u64, |total, message| {
                    total.saturating_add(message.estimated_tokens)
                });
            session.messages = import.messages;
            let quick_models = crate::config::quick_models_map(cfg);
            session.update_context_window(cfg.resolve_context_window(
                &session.provider,
                &session.model,
                &quick_models,
            ));
            if let Some(model) = quick_models
                .values()
                .find(|model| model.provider == session.provider && model.model == session.model)
            {
                session.input_token_cost = model.input_token_cost;
                session.output_token_cost = model.output_token_cost;
            } else if let Some((input, output)) =
                crate::config::Config::catalog_input_output_cost(&session.provider, &session.model)
            {
                session.input_token_cost = input;
                session.output_token_cost = output;
            }
            Ok(session)
        }
    }
}

#[cfg(feature = "export")]
async fn handle_share(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let filename = format!(
        "zerostack-session-{}.html",
        &ctx.session.id[..8.min(ctx.session.id.len())]
    );
    let html = crate::extras::export::session_to_html(ctx.session);
    let description = if ctx.session.name.is_empty() {
        "zerostack session".to_string()
    } else {
        format!("zerostack session: {}", ctx.session.name)
    };
    match crate::extras::export::share_gist(&filename, &html, &description).await {
        Ok(url) => write_ok(ctx.renderer, format!("shared as secret gist: {}", url)),
        Err(e) => write_error(ctx.renderer, format!("share failed: {}", e)),
    }
    Ok(())
}

async fn handle_rename(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 || parts[1].is_empty() {
        if ctx.session.name.is_empty() {
            write_ok(
                ctx.renderer,
                "current session has no name. Usage: /rename <name>",
            );
        } else {
            write_ok(
                ctx.renderer,
                format!(
                    "current session name: \"{}\". Usage: /rename <new-name>",
                    ctx.session.name
                ),
            );
        }
        return Ok(());
    }
    let new_name = parts[1..].join(" ").trim().to_string();
    ctx.session.name = CompactString::new(&new_name);
    crate::session::storage::save_session(ctx.session)?;
    write_ok(ctx.renderer, format!("session renamed to \"{}\"", new_name));
    Ok(())
}

async fn handle_sessions(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 {
        let sessions = crate::session::storage::find_recent_sessions(20)?;
        if sessions.is_empty() {
            write_ok(ctx.renderer, "no saved sessions");
        } else {
            write_ok(
                ctx.renderer,
                format!("recent sessions ({}):", sessions.len()),
            );
            for s in &sessions {
                write_result(ctx.renderer, format_session_line(s));
            }
        }
    } else if parts[1] == "delete" && parts.len() >= 3 {
        let prefix = parts[2].trim();
        let sessions = crate::session::storage::find_sessions_by_prefix(prefix)?;
        if sessions.is_empty() {
            write_ok(ctx.renderer, format!("no session matching '{}'", prefix));
        } else if sessions.len() == 1 {
            if let Some(s) = sessions.into_iter().next() {
                let id = s.id.clone();
                let preview = s
                    .messages
                    .last()
                    .map(|m| format!("...{}", m.content.chars().take(40).collect::<String>()))
                    .unwrap_or_default();
                if let Err(e) = crate::session::storage::delete_session(&id) {
                    write_error(ctx.renderer, format!("failed to delete: {}", e));
                } else {
                    write_ok(
                        ctx.renderer,
                        format!("deleted session {} {}", &id[..8], preview),
                    );
                }
            }
        } else {
            write_ok(
                ctx.renderer,
                format!("multiple sessions match '{}', be more specific", prefix),
            );
            for s in &sessions {
                write_result(ctx.renderer, format_session_line(s));
            }
        }
    } else {
        let prefix = parts[1].trim();
        let sessions = crate::session::storage::find_sessions_by_prefix(prefix)?;
        if sessions.is_empty() {
            write_ok(ctx.renderer, format!("no session matching '{}'", prefix));
        } else if sessions.len() == 1 {
            if let Some(s) = sessions.into_iter().next() {
                let msg_count = s.messages.len();
                *ctx.session = s;
                render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
                write_ok(ctx.renderer, format!("loaded session ({} msgs)", msg_count));
            }
        } else {
            write_ok(
                ctx.renderer,
                format!("multiple sessions match '{}':", prefix),
            );
            for s in &sessions {
                write_result(ctx.renderer, format_session_line(s));
            }
        }
    }
    Ok(())
}

async fn handle_clear(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    #[cfg(feature = "hooks")]
    crate::extras::hooks::dispatch_session_end("clear").await;
    ctx.session.messages.clear();
    ctx.session.total_estimated_tokens = 0;
    ctx.session.reset_calibration();
    ctx.session.compactions.clear();
    ctx.context.chain_declined.clear();
    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
    #[cfg(feature = "hooks")]
    crate::extras::hooks::dispatch_session_start("clear").await;
    Ok(())
}

async fn handle_undo(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let removed = undo_last(ctx.session);
    if removed == 0 {
        write_ok(ctx.renderer, "nothing to undo");
        return Ok(());
    }

    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
    write_ok(ctx.renderer, format!("removed {} message(s)", removed));

    write_ok(ctx.renderer, "  git stash working changes? [y/N] ");

    let mut buf = [0u8; 1];
    let do_stash =
        std::io::stdin().read_exact(&mut buf).is_ok() && (buf[0] == b'y' || buf[0] == b'Y');

    if do_stash {
        match std::process::Command::new("git").args(["stash"]).output() {
            Ok(out) if out.status.success() => {
                write_ok(ctx.renderer, "git stash done");
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                write_error(ctx.renderer, format!("git stash failed: {}", stderr.trim()));
            }
            Err(e) => {
                write_error(ctx.renderer, format!("git stash failed: {}", e));
            }
        }
    }

    Ok(())
}

async fn handle_redo(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if !ctx.session.redo() {
        write_ok(ctx.renderer, "nothing to redo");
        return Ok(());
    }
    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
    write_ok(ctx.renderer, "restored the last rewind");
    Ok(())
}

async fn handle_rewind(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let targets = crate::ui::rewind_targets(ctx.session);
    if targets.is_empty() {
        write_ok(ctx.renderer, "nothing to rewind to");
        return Ok(());
    }
    ctx.input.start_rewind_picker(targets);
    Ok(())
}

async fn handle_retry(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let last_user = ctx
        .session
        .messages
        .iter()
        .rev()
        .find(|m| m.role == crate::session::MessageRole::User)
        .cloned();
    match last_user {
        Some(msg) => {
            ctx.input.buffer = msg.content.clone();
            ctx.input.cursor = msg.content.len();
            write_ok(ctx.renderer, "edit last message and press Enter to retry");
        }
        None => {
            write_ok(ctx.renderer, "no previous message to retry");
        }
    }
    Ok(())
}

async fn handle_quit(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    *ctx.is_running = false;
    Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "quit").into())
}

async fn handle_history(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match crate::session::chat_history::load_history() {
        Ok(entries) => {
            if entries.is_empty() {
                write_ok(ctx.renderer, "no chat history");
            } else {
                write_ok(
                    ctx.renderer,
                    format!("global chat history ({} entries):", entries.len()),
                );
                for entry in entries.iter().rev().take(10).rev() {
                    let preview: String = entry.content.chars().take(80).collect();
                    write_result(ctx.renderer, format!("  {}", preview));
                }
                if entries.len() > 10 {
                    write_ok(ctx.renderer, "  ... (showing last 10)");
                }
            }
        }
        Err(e) => {
            write_error(ctx.renderer, format!("failed to load chat history: {}", e));
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "export"))]
mod import_tests {
    use super::{commit_staged_import, parse_imported_session};
    use crate::config::Config;
    use crate::extras::export::session_to_jsonl;
    use crate::session::{MessageRole, Session};
    use std::cell::RefCell;

    #[test]
    fn session_jsonl_slash_import_round_trip() {
        let mut exported = Session::new("source-provider", "source-model", 64_000, "source name");
        exported.created_at = "2026-07-29T12:00:00Z".into();
        exported.add_message(MessageRole::User, "first");
        exported.add_message(MessageRole::Assistant, "second");
        let jsonl = session_to_jsonl(&exported).unwrap();

        let current = Session::new("current-provider", "current-model", 128_000, "current");
        let cfg = Config::default();
        let imported = parse_imported_session(&jsonl, &current, &cfg).unwrap();
        assert_eq!(imported.id, exported.id);
        assert_eq!(imported.name, exported.name);
        assert_eq!(imported.provider, exported.provider);
        assert_eq!(imported.model, exported.model);
        assert_eq!(imported.created_at, exported.created_at);
        assert_eq!(
            imported.context_window,
            cfg.resolve_context_window(
                &exported.provider,
                &exported.model,
                &crate::config::quick_models_map(&cfg)
            )
        );
        assert_eq!(imported.working_dir, current.working_dir);
        assert_eq!(imported.messages.len(), exported.messages.len());
        for (imported, original) in imported.messages.iter().zip(&exported.messages) {
            assert_eq!(imported.role, original.role);
            assert_eq!(imported.content, original.content);
            assert_eq!(imported.estimated_tokens, original.estimated_tokens);
        }

        let persisted = serde_json::to_string(&imported).unwrap();
        let reloaded: Session = serde_json::from_str(&persisted).unwrap();
        assert_eq!(reloaded.id, exported.id);
        assert_eq!(reloaded.messages.len(), exported.messages.len());
    }

    #[test]
    fn native_session_import_remains_exact() {
        let mut native = Session::new("native-provider", "native-model", 32_000, "native");
        native.input_token_cost = 1.25;
        native.output_token_cost = 2.5;
        let current = Session::new("current", "current", 128_000, "current");
        let content = serde_json::to_string_pretty(&native).unwrap();
        let cfg = Config::default();
        let imported = parse_imported_session(&content, &current, &cfg).unwrap();
        assert_eq!(imported.id, native.id);
        assert_eq!(imported.provider, native.provider);
        assert_eq!(imported.context_window, native.context_window);
        assert_eq!(imported.input_token_cost, native.input_token_cost);
        assert_eq!(imported.output_token_cost, native.output_token_cost);
    }

    #[test]
    fn malformed_import_does_not_mutate_current_session() {
        let current = Session::new("current", "current", 128_000, "current");
        let original_id = current.id.clone();
        let original_messages = current.messages.len();

        let error = parse_imported_session("{\"id\":", &current, &Config::default())
            .err()
            .expect("malformed input must fail");
        assert!(error.to_string().contains("not valid JSON"));
        assert_eq!(current.id, original_id);
        assert_eq!(current.messages.len(), original_messages);
    }

    #[test]
    fn import_commit_is_save_first_and_transactional() {
        let mut session = String::from("old session");
        let mut client = String::from("old client");
        let mut agent = Some(String::from("old agent"));
        let error = commit_staged_import(
            &mut session,
            &mut client,
            &mut agent,
            String::from("new session"),
            String::from("new client"),
            String::from("new agent"),
            |_| anyhow::bail!("injected save failure"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected save failure"));
        assert_eq!(session, "old session");
        assert_eq!(client, "old client");
        assert_eq!(agent.as_deref(), Some("old agent"));

        let persisted = RefCell::new(None);
        commit_staged_import(
            &mut session,
            &mut client,
            &mut agent,
            String::from("new session"),
            String::from("new client"),
            String::from("new agent"),
            |candidate| {
                *persisted.borrow_mut() = Some(serde_json::to_string(candidate)?);
                Ok(())
            },
        )
        .unwrap();
        let reloaded: String =
            serde_json::from_str(persisted.borrow().as_deref().unwrap()).unwrap();
        assert_eq!(reloaded, "new session");
        assert_eq!(session, "new session");
        assert_eq!(client, "new client");
        assert_eq!(agent.as_deref(), Some("new agent"));
    }
}
