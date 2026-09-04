use compact_str::CompactString;

use crate::ui::events::render_session;
use crate::ui::slash::{SlashCtx, undo_last, write_error, write_ok, write_result};

/// Char-safe display prefix of a session id. Session ids are UUIDs, but
/// imported or hand-edited ids may be short or contain multi-byte characters;
/// byte slicing (`&id[..8]`) panics on both.
pub(crate) fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

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
        short_id(&s.id),
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
        "/undo" => handle_undo(parts, ctx).await,
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
    let default_name = format!("zerostack-session-{}.html", short_id(&ctx.session.id));
    let requested = parts
        .get(1)
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .unwrap_or(&default_name);
    let path = resolve_export_path(ctx.workspace.root(), requested);
    let (content, kind) = if path
        .extension()
        .is_some_and(|extension| extension == "jsonl")
    {
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
    match crate::fs::atomic_create_sync(&path, content.as_bytes()) {
        Ok(()) => write_ok(
            ctx.renderer,
            format!("exported {} to {}", kind, path.display()),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => write_error(
            ctx.renderer,
            format!(
                "export refused to overwrite existing file: {}",
                path.display()
            ),
        ),
        Err(e) => write_error(ctx.renderer, format!("export failed: {}", e)),
    }
    Ok(())
}

#[cfg(feature = "export")]
fn resolve_export_path(workspace_root: &std::path::Path, requested: &str) -> std::path::PathBuf {
    let requested = std::path::Path::new(requested);
    if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace_root.join(requested)
    }
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

    let mut session =
        match parse_imported_session(&content, ctx.session, ctx.cfg, ctx.workspace.root()) {
            Ok(session) => session,
            Err(error) => {
                write_error(ctx.renderer, format!("invalid session file: {}", error));
                return Ok(());
            }
        };

    if session.name.is_empty() {
        session.name = CompactString::new("imported");
    }
    session.initialize_read_tracker(ctx.cfg.deny_repeated_reads.unwrap_or(true));
    session.overhead_tokens = crate::agent::builder::estimate_overhead(
        ctx.context,
        *ctx.reasoning_enabled,
        ctx.cli,
        ctx.cfg,
        ctx.sandbox,
    );
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
        .build_agent_for_client(&new_client, &session.model, &session.read_tracker)
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
    use std::io::Read;

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

/// Imported session ids become on-disk file names and are displayed by an
/// 8-character prefix, so only accept ids shaped like the UUIDs zerostack
/// generates itself (36 chars, hyphenated hex).
#[cfg_attr(not(feature = "export"), allow(dead_code))]
fn validate_imported_session_id(id: &str) -> anyhow::Result<()> {
    if uuid::Uuid::try_parse(id).is_ok() && id.len() == 36 && id.is_ascii() {
        return Ok(());
    }
    anyhow::bail!(
        "session id {:?} is not a UUID (expected 36 hyphenated hex characters, e.g. 123e4567-e89b-12d3-a456-426614174000)",
        id.chars().take(48).collect::<String>()
    )
}

#[cfg(feature = "export")]
fn parse_imported_session(
    content: &str,
    current: &crate::session::Session,
    cfg: &crate::config::Config,
    workspace: &std::path::Path,
) -> anyhow::Result<crate::session::Session> {
    match crate::extras::export::parse_session_file(content)? {
        crate::extras::export::ParsedSessionFile::Native(mut session) => {
            validate_imported_session_id(&session.id)?;
            // Native imports are external input, unlike private storage reloads.
            // Never accept a concealed redo payload that is absent from the
            // visible top-level history.
            session.rewind_undo = None;
            session.working_dir = workspace.to_string_lossy().into_owned().into();
            Ok(session)
        }
        crate::extras::export::ParsedSessionFile::Jsonl(import) => {
            validate_imported_session_id(&import.id)?;
            crate::paths::validate_portable_component(&import.id)?;
            let mut session = crate::session::Session::new(
                import.provider.as_str(),
                import.model.as_str(),
                current.context_window,
                import.name.as_str(),
            );
            session.id = import.id;
            session.created_at = import.created_at;
            session.working_dir = workspace.to_string_lossy().into_owned().into();
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
                        format!("deleted session {} {}", short_id(&id), preview),
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
                ctx.replace_session(s).await?;
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
    match mutate_and_persist_session(
        ctx.session,
        clear_session_mutation,
        |session| persist_session_unless_ephemeral(ctx.cli.no_session, session),
        crate::session::storage::load_session_exact,
    ) {
        Ok(PersistedMutation::Persisted(())) => {}
        Ok(PersistedMutation::PersistedWithWarning((), warning)) => {
            write_error(ctx.renderer, warning);
        }
        Ok(PersistedMutation::Unchanged) => unreachable!("clear always changes the session"),
        Err(error) => {
            write_error(
                ctx.renderer,
                format!("failed to persist cleared session: {error}"),
            );
            if is_persistence_restart_required(&error) {
                return Err(error);
            }
            return Ok(());
        }
    }
    #[cfg(feature = "hooks")]
    crate::extras::hooks::dispatch_session_end("clear").await;
    ctx.context.chain_declined.clear();
    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
    #[cfg(feature = "hooks")]
    crate::extras::hooks::dispatch_session_start("clear").await;
    Ok(())
}

async fn handle_undo(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    // The stash decision is taken from the command line rather than from a
    // blocking stdin prompt: the crossterm event thread owns the tty while the
    // TUI runs, so any synchronous stdin read here either freezes the UI or
    // has its answer key stolen. Default is the safe choice (no stash).
    let stash = match parts.get(1).map(|p| p.trim()) {
        None | Some("") => false,
        Some("stash") => true,
        Some(other) => {
            write_error(
                ctx.renderer,
                format!("unknown /undo option '{}' (usage: /undo [stash])", other),
            );
            return Ok(());
        }
    };
    let removed = match mutate_and_persist_session(
        ctx.session,
        undo_session_mutation,
        |session| persist_session_unless_ephemeral(ctx.cli.no_session, session),
        crate::session::storage::load_session_exact,
    ) {
        Ok(PersistedMutation::Persisted(removed)) => removed,
        Ok(PersistedMutation::PersistedWithWarning(removed, warning)) => {
            write_error(ctx.renderer, warning);
            removed
        }
        Ok(PersistedMutation::Unchanged) => {
            write_ok(ctx.renderer, "nothing to undo");
            return Ok(());
        }
        Err(error) => {
            write_error(ctx.renderer, format!("failed to persist undo: {error}"));
            if is_persistence_restart_required(&error) {
                return Err(error);
            }
            return Ok(());
        }
    };
    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
    write_ok(ctx.renderer, format!("removed {} message(s)", removed));

    if !stash {
        write_ok(
            ctx.renderer,
            "working tree left untouched (use `/undo stash` to also git stash changes)",
        );
    } else {
        match crate::ui::git_stash_in_workspace(ctx.workspace.root()) {
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
    match mutate_and_persist_session(
        ctx.session,
        redo_session_mutation,
        |session| persist_session_unless_ephemeral(ctx.cli.no_session, session),
        crate::session::storage::load_session_exact,
    ) {
        Ok(PersistedMutation::Persisted(())) => {}
        Ok(PersistedMutation::PersistedWithWarning((), warning)) => {
            write_error(ctx.renderer, warning);
        }
        Ok(PersistedMutation::Unchanged) => {
            write_ok(ctx.renderer, "nothing to redo");
            return Ok(());
        }
        Err(error) => {
            write_error(ctx.renderer, format!("failed to persist redo: {error}"));
            if is_persistence_restart_required(&error) {
                return Err(error);
            }
            return Ok(());
        }
    }
    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
    write_ok(ctx.renderer, "restored the last rewind");
    Ok(())
}

fn clear_session_mutation(session: &mut crate::session::Session) -> Option<()> {
    session.messages.clear();
    session.total_estimated_tokens = 0;
    session.reset_calibration();
    session.compactions.clear();
    session.rewind_undo = None;
    session.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
    Some(())
}

fn undo_session_mutation(session: &mut crate::session::Session) -> Option<usize> {
    let removed = undo_last(session);
    if removed == 0 {
        None
    } else {
        session.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
        Some(removed)
    }
}

fn redo_session_mutation(session: &mut crate::session::Session) -> Option<()> {
    if session.redo() {
        session.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
        Some(())
    } else {
        None
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PersistedMutation<T> {
    Unchanged,
    Persisted(T),
    PersistedWithWarning(T, String),
}

fn mutate_and_persist_session<T>(
    session: &mut crate::session::Session,
    mutate: impl FnOnce(&mut crate::session::Session) -> Option<T>,
    persist: impl FnOnce(&crate::session::Session) -> anyhow::Result<()>,
    load_persisted: impl FnOnce(&str) -> anyhow::Result<Option<crate::session::Session>>,
) -> anyhow::Result<PersistedMutation<T>> {
    let previous = session.clone();
    let Some(result) = mutate(session) else {
        return Ok(PersistedMutation::Unchanged);
    };
    if let Err(error) = persist(session) {
        let candidate = serde_json::to_vec(session).map_err(|serialization_error| {
            *session = previous.clone();
            persistence_restart_required(format!(
                "save failed ({error}); candidate reconciliation failed ({serialization_error}); previous in-memory state was restored but disk state is unknown—restart before continuing"
            ))
        })?;
        let previous_wire = serde_json::to_vec(&previous).map_err(|serialization_error| {
            *session = previous.clone();
            persistence_restart_required(format!(
                "save failed ({error}); previous-state reconciliation failed ({serialization_error}); previous in-memory state was restored but disk state is unknown—restart before continuing"
            ))
        })?;
        match load_persisted(&session.id) {
            Ok(Some(on_disk)) => {
                let on_disk_wire = serde_json::to_vec(&on_disk).map_err(|serialization_error| {
                    *session = previous.clone();
                    persistence_restart_required(format!(
                        "save failed ({error}); persisted-state reconciliation failed ({serialization_error}); previous in-memory state was restored but disk state is unknown—restart before continuing"
                    ))
                })?;
                if on_disk_wire == candidate {
                    return Ok(PersistedMutation::PersistedWithWarning(
                        result,
                        format!(
                            "session mutation was committed, but post-commit verification reported an error; live state was retained to match disk: {error}"
                        ),
                    ));
                }
                if on_disk_wire == previous_wire {
                    *session = previous;
                    anyhow::bail!(
                        "save failed before commit; previous session was restored: {error}"
                    );
                }
                *session = previous;
                return Err(persistence_restart_required(format!(
                    "save failed and disk contains a different session state ({error}); the previous live state was retained without autosaving and zerostack must restart to load the authoritative disk state"
                )));
            }
            Ok(None) => {
                *session = previous;
                anyhow::bail!(
                    "save failed and no persisted session was found; previous in-memory state was restored: {error}"
                );
            }
            Err(reconcile_error) => {
                *session = previous;
                return Err(persistence_restart_required(format!(
                    "save failed ({error}); disk reconciliation also failed ({reconcile_error}); previous in-memory state was restored without autosaving and zerostack must restart before continuing"
                )));
            }
        }
    }
    Ok(PersistedMutation::Persisted(result))
}

#[derive(Debug)]
struct PersistenceRestartRequired(String);

impl std::fmt::Display for PersistenceRestartRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PersistenceRestartRequired {}

fn persistence_restart_required(message: String) -> anyhow::Error {
    PersistenceRestartRequired(message).into()
}

pub(crate) fn is_persistence_restart_required(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PersistenceRestartRequired>().is_some()
}

fn persist_session_unless_ephemeral(
    no_session: bool,
    session: &crate::session::Session,
) -> anyhow::Result<()> {
    if no_session {
        Ok(())
    } else {
        crate::session::storage::save_session(session)
    }
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
            ctx.input.load_text(&msg.content);
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
    use super::{commit_staged_import, parse_imported_session, resolve_export_path};
    use crate::config::Config;
    use crate::extras::export::session_to_jsonl;
    use crate::session::{MessageRole, Session};
    use std::cell::RefCell;
    use std::path::Path;

    #[test]
    fn relative_exports_resolve_against_the_active_workspace() {
        assert_eq!(
            resolve_export_path(Path::new("/active-worktree"), "session.html"),
            Path::new("/active-worktree/session.html")
        );
        assert_eq!(
            resolve_export_path(Path::new("/active-worktree"), "/tmp/session.html"),
            Path::new("/tmp/session.html")
        );
    }

    #[test]
    fn export_publication_is_create_only() {
        let directory = std::env::temp_dir().join(format!(
            "mini-agent-export-create-only-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("session.html");
        crate::fs::atomic_create_sync(&path, b"first").unwrap();
        let error = crate::fs::atomic_create_sync(&path, b"second").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn session_jsonl_slash_import_round_trip() {
        let mut exported = Session::new("source-provider", "source-model", 64_000, "source name");
        exported.created_at = "2026-07-29T12:00:00Z".into();
        exported.add_message(MessageRole::User, "first");
        exported.add_message(MessageRole::Assistant, "second");
        let jsonl = session_to_jsonl(&exported).unwrap();

        let mut current = Session::new("current-provider", "current-model", 128_000, "current");
        current.working_dir = "stale-session-workspace".into();
        let workspace = Path::new("active-workspace");
        let cfg = Config::default();
        let imported = parse_imported_session(&jsonl, &current, &cfg, workspace).unwrap();
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
        assert_eq!(Path::new(imported.working_dir.as_str()), workspace);
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
        native.working_dir = "/saved/workspace-b".into();
        let mut current = Session::new("current", "current", 128_000, "current");
        current.working_dir = "stale-session-workspace".into();
        let workspace = Path::new("active-workspace");
        let content = serde_json::to_string_pretty(&native).unwrap();
        let cfg = Config::default();
        let imported = parse_imported_session(&content, &current, &cfg, workspace).unwrap();
        assert_eq!(imported.id, native.id);
        assert_eq!(imported.provider, native.provider);
        assert_eq!(imported.context_window, native.context_window);
        assert_eq!(imported.input_token_cost, native.input_token_cost);
        assert_eq!(imported.output_token_cost, native.output_token_cost);
        assert_eq!(Path::new(imported.working_dir.as_str()), workspace);
    }

    #[test]
    fn native_session_import_discards_concealed_redo_history() {
        let mut native = Session::new("native-provider", "native-model", 32_000, "native");
        native.add_message(MessageRole::User, "concealed");
        native.add_message(MessageRole::Assistant, "history");
        assert_eq!(native.rewind_to(0), 2);
        assert!(native.messages.is_empty());
        assert!(native.rewind_undo.is_some());

        let current = Session::new("current", "current", 128_000, "current");
        let imported = parse_imported_session(
            &serde_json::to_string(&native).unwrap(),
            &current,
            &Config::default(),
            Path::new("active-workspace"),
        )
        .unwrap();
        assert!(imported.messages.is_empty());
        assert!(imported.rewind_undo.is_none());
    }

    #[test]
    fn malformed_import_does_not_mutate_current_session() {
        let current = Session::new("current", "current", 128_000, "current");
        let original_id = current.id.clone();
        let original_messages = current.messages.len();

        let error = parse_imported_session(
            "{\"id\":",
            &current,
            &Config::default(),
            Path::new("active-workspace"),
        )
        .expect_err("malformed input must fail");
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

#[cfg(test)]
mod session_id_tests {
    use super::{short_id, validate_imported_session_id};

    #[test]
    fn short_id_is_char_safe_for_short_and_non_ascii_ids() {
        assert_eq!(short_id("123e4567-e89b-12d3-a456-426614174000"), "123e4567");
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
        // 8 chars, not 8 bytes: byte slicing would panic inside 'é'.
        assert_eq!(short_id("éééééééééé"), "éééééééé");
        assert_eq!(short_id("日本語テスト"), "日本語テスト");
    }

    #[test]
    fn imported_session_id_must_be_uuid_shaped() {
        assert!(validate_imported_session_id("123e4567-e89b-12d3-a456-426614174000").is_ok());
        assert!(validate_imported_session_id("123E4567-E89B-12D3-A456-426614174000").is_ok());
        for bad in [
            "",
            "abc",
            "123e4567e89b12d3a456426614174000",
            "../../etc/passwd",
            "日本語テスト-e89b-12d3-a456-426614174000",
            "{123e4567-e89b-12d3-a456-426614174000}",
            "urn:uuid:123e4567-e89b-12d3-a456-426614174000",
        ] {
            let error = validate_imported_session_id(bad).unwrap_err().to_string();
            assert!(error.contains("not a UUID"), "{bad}: {error}");
        }
    }
}

#[cfg(test)]
mod session_persistence_tests {
    use super::{
        PersistedMutation, clear_session_mutation, is_persistence_restart_required,
        mutate_and_persist_session, redo_session_mutation, undo_session_mutation,
    };
    use crate::session::{MessageRole, Session};
    use std::cell::{Cell, RefCell};

    fn session_with_turn() -> Session {
        let mut session = Session::new("provider", "model", 128_000, "persistence");
        session.add_message(MessageRole::User, "question");
        session.add_message(MessageRole::Assistant, "answer");
        session
    }

    fn reload(snapshot: &RefCell<Option<String>>) -> Session {
        serde_json::from_str(snapshot.borrow().as_deref().expect("snapshot must exist")).unwrap()
    }

    #[test]
    fn slash_session_mutation_persistence_survives_immediate_reload() {
        let snapshot = RefCell::new(None);
        let mut session = session_with_turn();
        let original_messages = session.messages.clone();

        let removed = mutate_and_persist_session(
            &mut session,
            undo_session_mutation,
            |candidate| {
                *snapshot.borrow_mut() = Some(serde_json::to_string(candidate)?);
                Ok(())
            },
            |_| Ok(None),
        )
        .unwrap();
        assert_eq!(removed, PersistedMutation::Persisted(2));
        assert!(session.messages.is_empty());
        let mut reloaded_undo = reload(&snapshot);
        assert!(reloaded_undo.messages.is_empty());
        assert!(reloaded_undo.redo(), "restart must retain the redo point");
        assert_eq!(reloaded_undo.messages.len(), original_messages.len());

        mutate_and_persist_session(
            &mut session,
            redo_session_mutation,
            |candidate| {
                *snapshot.borrow_mut() = Some(serde_json::to_string(candidate)?);
                Ok(())
            },
            |_| Ok(None),
        )
        .unwrap();
        let mut reloaded_redo = reload(&snapshot);
        assert_eq!(reloaded_redo.messages.len(), original_messages.len());
        assert!(!reloaded_redo.redo(), "a consumed redo must stay consumed");

        mutate_and_persist_session(
            &mut session,
            undo_session_mutation,
            |_| Ok(()),
            |_| Ok(None),
        )
        .unwrap();
        assert!(session.rewind_undo.is_some());
        mutate_and_persist_session(
            &mut session,
            clear_session_mutation,
            |candidate| {
                *snapshot.borrow_mut() = Some(serde_json::to_string(candidate)?);
                Ok(())
            },
            |_| Ok(None),
        )
        .unwrap();
        let mut reloaded_clear = reload(&snapshot);
        assert!(reloaded_clear.messages.is_empty());
        assert_eq!(reloaded_clear.total_estimated_tokens, 0);
        assert_eq!(reloaded_clear.calibrated_tokens, 0);
        assert_eq!(reloaded_clear.calibrated_msg_count, 0);
        assert!(reloaded_clear.compactions.is_empty());
        assert!(
            !reloaded_clear.redo(),
            "clear must invalidate the redo point"
        );
    }

    #[test]
    fn slash_session_mutation_persistence_rolls_back_failed_save() {
        let mut session = session_with_turn();
        let persisted_before = session.clone();
        let before = serde_json::to_string(&session).unwrap();
        let error = mutate_and_persist_session(
            &mut session,
            undo_session_mutation,
            |_| anyhow::bail!("injected persistence failure"),
            |_| Ok(Some(persisted_before)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected persistence failure"));
        assert_eq!(serde_json::to_string(&session).unwrap(), before);
        assert!(session.rewind_undo.is_none());
    }

    #[test]
    fn slash_session_mutation_persistence_retains_post_commit_state() {
        let mut session = session_with_turn();
        let committed = RefCell::new(None);
        let outcome = mutate_and_persist_session(
            &mut session,
            undo_session_mutation,
            |candidate| {
                *committed.borrow_mut() = Some(candidate.clone());
                anyhow::bail!("injected post-commit verification failure")
            },
            |_| Ok(committed.borrow().clone()),
        )
        .unwrap();
        let PersistedMutation::PersistedWithWarning(2, warning) = outcome else {
            panic!("committed candidate must be retained with a warning");
        };
        assert!(warning.contains("post-commit verification"));
        assert!(session.messages.is_empty());
        assert!(session.rewind_undo.is_some());
    }

    #[test]
    fn slash_session_mutation_persistence_requires_restart_for_divergent_disk_state() {
        let mut session = session_with_turn();
        let previous_wire = serde_json::to_string(&session).unwrap();
        let mut divergent = session.clone();
        divergent.name = "concurrent disk writer".into();
        let error = mutate_and_persist_session(
            &mut session,
            undo_session_mutation,
            |_| anyhow::bail!("injected ambiguous save failure"),
            |_| Ok(Some(divergent)),
        )
        .unwrap_err();
        assert!(is_persistence_restart_required(&error));
        assert!(error.to_string().contains("must restart"));
        assert_eq!(serde_json::to_string(&session).unwrap(), previous_wire);
    }

    #[test]
    fn slash_session_mutation_persistence_skips_empty_undo_and_redo() {
        let mut session = Session::new("provider", "model", 128_000, "empty");
        let persisted = Cell::new(false);
        let undo = mutate_and_persist_session(
            &mut session,
            undo_session_mutation,
            |_| {
                persisted.set(true);
                Ok(())
            },
            |_| Ok(None),
        )
        .unwrap();
        assert_eq!(undo, PersistedMutation::Unchanged);
        assert!(!persisted.get());

        let redo = mutate_and_persist_session(
            &mut session,
            redo_session_mutation,
            |_| {
                persisted.set(true);
                Ok(())
            },
            |_| Ok(None),
        )
        .unwrap();
        assert_eq!(redo, PersistedMutation::Unchanged);
        assert!(!persisted.get());
    }

    #[test]
    fn slash_session_mutation_persistence_supports_repeated_undo_redo_cycles() {
        let mut session = session_with_turn();
        for _ in 0..3 {
            assert_eq!(
                mutate_and_persist_session(
                    &mut session,
                    undo_session_mutation,
                    |_| Ok(()),
                    |_| Ok(None),
                )
                .unwrap(),
                PersistedMutation::Persisted(2)
            );
            assert!(session.messages.is_empty());
            assert_eq!(
                mutate_and_persist_session(
                    &mut session,
                    redo_session_mutation,
                    |_| Ok(()),
                    |_| Ok(None),
                )
                .unwrap(),
                PersistedMutation::Persisted(())
            );
            assert_eq!(session.messages.len(), 2);
        }
    }

    #[test]
    fn slash_session_mutation_persistence_compaction_invalidates_redo_across_restart() {
        let mut session = session_with_turn();
        assert_eq!(
            mutate_and_persist_session(
                &mut session,
                undo_session_mutation,
                |_| Ok(()),
                |_| Ok(None)
            )
            .unwrap(),
            PersistedMutation::Persisted(2)
        );
        assert!(session.rewind_undo.is_some());
        // Compaction mutates/reindexes the visible history after undo.
        session.compress("summary".to_string(), 0, 0);
        assert!(session.rewind_undo.is_none());

        let serialized = serde_json::to_string(&session).unwrap();
        let mut reloaded: Session = serde_json::from_str(&serialized).unwrap();
        assert!(!reloaded.redo());
        assert_eq!(reloaded.compacted_context().0, Some("summary"));
    }
}
