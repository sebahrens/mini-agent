//! Persona routing for ACP sessions.
//!
//! An ACP session selects its main-loop prompt the way interactive startup
//! does: `--load-prompt`, else `default_prompt`, else the embedded `code`
//! prompt, activated through [`ContextFiles::activate_main_prompt`]. The prompt's
//! `%%agent=` directive selects the main-agent persona, its `%%mode=` directive
//! may only narrow the per-turn permission checker
//! ([`crate::permission::checker::PermissionChecker::set_prompt_mode`]), and its
//! `[prompt_to_model]` route ([`Config::resolve_prompt_route`]) selects the
//! turn's provider and model.
//!
//! Clients see the available prompts as ACP session modes and as an equivalent
//! `mode` session config option, and switch with `session/set_mode` or
//! `session/set_config_option`. Unlike the TUI, a route that cannot be honoured
//! is never replaced by another provider: a switch to it is refused, and a turn
//! whose route cannot be initialized fails with a bounded, credential-free
//! error.

use compact_str::CompactString;

use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
};
#[cfg(test)]
use agent_client_protocol::schema::v1::{SessionMode, SessionModeState};

use crate::cli::Cli;
use crate::config::{Config, PromptRoute};
use crate::context::ContextFiles;
use crate::permission::checker::PermCheck;

/// The mode id that selects no prompt: the session runs on the startup
/// provider/model with the operator-configured permission mode, as ACP did
/// before persona routing. Prompt names are file stems, and `:` is not a
/// portable file-name character (Windows rejects it), so prompts do not use
/// this id; a Unix prompt file named `mini-agent:none.md` is not selectable.
pub(super) const NO_PROMPT_MODE_ID: &str = "mini-agent:none";

/// The session config option that mirrors the session mode selector.
pub(super) const PERSONA_CONFIG_ID: &str = "mode";

/// At most this many prompts are advertised. Every loaded prompt stays
/// selectable by id; only the advertised list is bounded.
const MAX_ADVERTISED_PROMPTS: usize = 128;

/// Upper bound for one error or description string sent to a client.
const MAX_MESSAGE_BYTES: usize = 512;

/// The provider and model one ACP turn runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TurnRoute {
    pub(super) provider: CompactString,
    pub(super) model: CompactString,
    /// The `[prompt_to_model]` alias and the prompt that selected it, when the
    /// route came from the session's prompt rather than the startup selection.
    pub(super) mapped: Option<(String, String)>,
}

/// Truncate `text` to [`MAX_MESSAGE_BYTES`] on a character boundary and drop
/// control characters, so a provider error cannot flood or corrupt a client.
pub(super) fn bounded_message(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_MESSAGE_BYTES));
    for ch in text.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if out.len() + ch.len_utf8() > MAX_MESSAGE_BYTES {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// The prompt a new ACP session starts with. `--load-prompt` must name a
/// loaded prompt (startup refuses an unknown one too); an absent
/// `default_prompt` leaves the session without a prompt, as at startup.
pub(super) fn initial_prompt(
    cli: &Cli,
    cfg: &Config,
    base: &ContextFiles,
) -> Result<Option<String>, String> {
    if let Some(name) = cli.load_prompt.as_deref() {
        if base.prompts.contains_key(name) {
            return Ok(Some(name.to_string()));
        }
        return Err(bounded_message(&format!(
            "unknown prompt '{name}' named by --load-prompt"
        )));
    }
    let name = cfg.resolve_default_prompt();
    if base.prompts.contains_key(name) {
        Ok(Some(name.to_string()))
    } else {
        tracing::warn!(
            prompt = name,
            "ACP: default prompt is not loaded; sessions start without a prompt"
        );
        Ok(None)
    }
}

/// The session context for `prompt`, activated on a fresh copy of the
/// session's unactivated `base` context so the selected prompt alone decides
/// the persona. `None` selects no prompt.
pub(super) fn activate(base: &ContextFiles, prompt: Option<&str>) -> Result<ContextFiles, String> {
    let mut context = base.clone();
    if let Some(name) = prompt
        && context.activate_main_prompt(name).is_none()
    {
        return Err(bounded_message(&format!(
            "unknown ACP session mode '{name}'"
        )));
    }
    Ok(context)
}

/// The mode id of the session's current selection.
pub(super) fn current_mode_id(context: &ContextFiles) -> &str {
    context
        .current_prompt_name
        .as_deref()
        .unwrap_or(NO_PROMPT_MODE_ID)
}

/// Translate a client-supplied mode or config value into a prompt selection.
pub(super) fn selection_for(mode_id: &str) -> Option<&str> {
    (mode_id != NO_PROMPT_MODE_ID).then_some(mode_id)
}

/// Resolve the provider/model for a turn in `context`: the active prompt's
/// `[prompt_to_model]` route, or the startup `--provider`/`--model`/
/// `--quick-model` selection when the prompt is unmapped. A route naming no
/// quick model is refused rather than silently ignored.
pub(super) fn resolve_route(
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
) -> Result<TurnRoute, String> {
    let Some(prompt) = context.current_prompt_name.as_deref() else {
        return Ok(startup_route(cli, cfg));
    };
    match cfg.resolve_prompt_route(prompt) {
        PromptRoute::Unmapped => Ok(startup_route(cli, cfg)),
        PromptRoute::Mapped { alias, quick_model } => Ok(TurnRoute {
            provider: quick_model.provider.clone(),
            model: quick_model.model.clone(),
            mapped: Some((prompt.to_string(), alias.to_string())),
        }),
        PromptRoute::Dangling { alias } => Err(bounded_message(&format!(
            "ACP prompt '{prompt}' routes to quick model '{alias}', which is not configured; \
             refusing to substitute another model"
        ))),
    }
}

fn startup_route(cli: &Cli, cfg: &Config) -> TurnRoute {
    let (provider, model) = super::acp_provider_and_model(cli, cfg);
    TurnRoute {
        provider,
        model,
        mapped: None,
    }
}

/// Check that the route's provider and credential resolve, without building
/// the HTTP client. A failure is never answered with another provider: the
/// message names the provider and the prompt that selected it but carries no
/// credential (provider errors name environment variables, never their
/// values) and is bounded.
pub(super) fn check_route(cli: &Cli, cfg: &Config, route: &TurnRoute) -> Result<(), String> {
    crate::provider::check_client_credentials(
        &route.provider,
        cli.api_key.as_deref(),
        cfg.custom_providers_map(),
        cfg.api_keys.as_ref(),
    )
    .map_err(|error| route_error(route, &error))
}

/// Create the turn's provider client, refusing as [`check_route`] does. The
/// client is served from the session's provider cache, so repeated turns on
/// the same provider reuse one connection pool instead of rebuilding it.
pub(super) fn route_client(
    cli: &Cli,
    cfg: &Config,
    route: &TurnRoute,
    clients: &super::ProviderClients,
) -> Result<crate::provider::AnyClient, String> {
    clients
        .get(cli, cfg, &route.provider)
        .map_err(|error| route_error(route, &error))
}

fn route_error(route: &TurnRoute, error: &anyhow::Error) -> String {
    match &route.mapped {
        Some((prompt, alias)) => bounded_message(&format!(
            "ACP prompt '{prompt}' routes to provider '{}' (quick model '{alias}'), which cannot \
             be initialized; refusing to substitute another provider: {error}",
            route.provider
        )),
        None => bounded_message(&error.to_string()),
    }
}

/// Apply the active prompt's `%%mode=` directive to a turn's permission
/// checker. The checker refuses any directive above the operator-configured
/// mode, so a persona can only narrow authority. When tools are enabled but
/// permission checks were disabled (`--dangerously-skip-permissions`), a
/// directive that names a mode cannot be enforced, so the turn is refused
/// instead of running with more authority than the persona allows.
pub(super) fn apply_permission_mode(
    permission: &Option<PermCheck>,
    tools_enabled: bool,
    context: &ContextFiles,
) -> Result<(), String> {
    let Some(prompt) = context.current_prompt_name.as_deref() else {
        return Ok(());
    };
    let Some(mode) = crate::permission::resolve_startup_prompt_mode(&context.prompts, prompt)
    else {
        return Ok(());
    };
    match permission {
        Some(permission) => {
            let mut guard = permission.lock().unwrap_or_else(|e| e.into_inner());
            if !guard.set_prompt_mode(mode) {
                tracing::info!(
                    "ACP prompt '{prompt}' requested mode {mode}; keeping operator-selected {}",
                    guard.mode()
                );
            }
            Ok(())
        }
        None if tools_enabled => Err(bounded_message(&format!(
            "ACP prompt '{prompt}' requires permission mode {mode}, but permission checks are \
             disabled; refusing the turn"
        ))),
        None => Ok(()),
    }
}

fn mode_description(cli: &Cli, cfg: &Config, base: &ContextFiles, prompt: &str) -> String {
    let mode = crate::permission::parse_prompt_mode(
        base.prompts.get(prompt).map(String::as_str).unwrap_or(""),
    )
    .0
    .unwrap_or("last_user_mode");
    let route = match cfg.resolve_prompt_route(prompt) {
        PromptRoute::Unmapped => {
            let route = startup_route(cli, cfg);
            format!("{}/{}", route.provider, route.model)
        }
        PromptRoute::Mapped { quick_model, .. } => {
            format!("{}/{}", quick_model.provider, quick_model.model)
        }
        PromptRoute::Dangling { alias } => format!("unconfigured quick model '{alias}'"),
    };
    bounded_message(&format!("model: {route}; permission mode: {mode}"))
}

/// The advertised selections: no prompt, then every loaded prompt by name.
fn advertised(cli: &Cli, cfg: &Config, base: &ContextFiles) -> Vec<(String, String, String)> {
    let startup = startup_route(cli, cfg);
    let mut names: Vec<&String> = base.prompts.keys().collect();
    names.sort();
    if names.len() > MAX_ADVERTISED_PROMPTS {
        tracing::warn!(
            loaded = names.len(),
            advertised = MAX_ADVERTISED_PROMPTS,
            "ACP: advertising only the first prompts as session modes"
        );
        names.truncate(MAX_ADVERTISED_PROMPTS);
    }
    let mut entries = Vec::with_capacity(names.len() + 1);
    entries.push((
        NO_PROMPT_MODE_ID.to_string(),
        "No prompt".to_string(),
        bounded_message(&format!(
            "model: {}/{}; permission mode: operator-selected",
            startup.provider, startup.model
        )),
    ));
    for name in names {
        entries.push((
            name.clone(),
            name.clone(),
            mode_description(cli, cfg, base, name),
        ));
    }
    entries
}

/// The session mode state advertised for a session in `context`.
#[cfg(test)]
pub(super) fn session_modes(
    cli: &Cli,
    cfg: &Config,
    base: &ContextFiles,
    context: &ContextFiles,
) -> SessionModeState {
    let modes = advertised(cli, cfg, base)
        .into_iter()
        .map(|(id, name, description)| SessionMode::new(id, name).description(description))
        .collect();
    SessionModeState::new(current_mode_id(context).to_string(), modes)
}

/// The session config options advertised for a session in `context`: one
/// `mode` selector mirroring [`session_modes`].
pub(super) fn session_config_options(
    cli: &Cli,
    cfg: &Config,
    base: &ContextFiles,
    context: &ContextFiles,
) -> Vec<SessionConfigOption> {
    let options: Vec<SessionConfigSelectOption> = advertised(cli, cfg, base)
        .into_iter()
        .map(|(id, name, description)| {
            SessionConfigSelectOption::new(id, name).description(description)
        })
        .collect();
    vec![
        SessionConfigOption::select(
            PERSONA_CONFIG_ID,
            "Persona",
            current_mode_id(context).to_string(),
            options,
        )
        .description(
            "Prompt and main-agent persona; routes the model through [prompt_to_model]".to_string(),
        )
        .category(SessionConfigOptionCategory::Mode),
    ]
}
