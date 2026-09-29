use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::cli::Cli;
use crate::config::{self, Config};
use crate::provider::{AnyClient, ModelEntry, list_models_manual};
use crate::ui::slash::{SlashCtx, write_error, write_ok, write_result};

pub async fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match parts[0] {
        "/provider" => handle_provider(parts, ctx).await,
        "/model" => handle_model(parts, ctx).await,
        "/models" => handle_models(parts, ctx).await,
        "/models-add" => handle_models_add(parts, ctx).await,
        name => match subagent_model_command(name) {
            Some(command) => handle_subagent_model_command(command, parts, ctx).await,
            None => Ok(()),
        },
    }
}

/// The subagent model commands, whatever spelling was typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubagentModelCommand {
    /// Show or switch the subagent model.
    Model,
    /// List quick models or switch the subagent to one.
    Models,
}

/// Recognise a subagent model command. `/subagent-model` and
/// `/subagent-models` are the canonical names; `/model-subagent` and
/// `/models-subagent` stay as hidden aliases (not offered by completion or
/// `/help`). Routed in every build so a build without the `subagents`
/// feature reports why it does nothing.
pub(crate) fn subagent_model_command(name: &str) -> Option<SubagentModelCommand> {
    match name {
        "/subagent-model" | "/model-subagent" => Some(SubagentModelCommand::Model),
        "/subagent-models" | "/models-subagent" => Some(SubagentModelCommand::Models),
        _ => None,
    }
}

/// Shown when a subagent model command runs in a build without subagents.
#[cfg(any(test, not(feature = "subagents")))]
pub(crate) const SUBAGENTS_DISABLED: &str = "subagent commands require the 'subagents' feature: cargo install --path . --features subagents";

async fn handle_subagent_model_command(
    command: SubagentModelCommand,
    parts: &[&str],
    ctx: &mut SlashCtx<'_>,
) -> anyhow::Result<()> {
    #[cfg(feature = "subagents")]
    {
        match command {
            SubagentModelCommand::Model => handle_model_subagent(parts, ctx).await,
            SubagentModelCommand::Models => handle_models_subagent(parts, ctx).await,
        }
    }
    #[cfg(not(feature = "subagents"))]
    {
        let _ = (command, parts);
        write_error(ctx.renderer, SUBAGENTS_DISABLED);
        Ok(())
    }
}

static MODEL_CACHE: LazyLock<Mutex<HashMap<String, Arc<[ModelEntry]>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bumped whenever [`MODEL_CACHE`] gains or replaces an entry, so the UI can
/// pick up a background warm without re-reading the list on every event.
static MODEL_CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Providers whose last listing failed, and when. A failed listing is not
/// retried implicitly until [`LISTING_FAILURE_TTL`] passes (an explicit
/// `/models refresh` always retries), so an unreachable gateway cannot stall
/// every slash command.
static LISTING_FAILURES: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Providers with a background warm in flight.
static WARMING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

const LISTING_FAILURE_TTL: Duration = Duration::from_secs(300);

/// Credentials a model listing needs, owned so a background task can use them.
#[derive(Clone)]
pub(crate) struct ListingCredentials {
    api_key: Option<String>,
    custom_providers: HashMap<String, crate::config::CustomProviderConfig>,
    api_keys: Option<HashMap<String, String>>,
}

impl ListingCredentials {
    pub(crate) fn new(cli: &Cli, cfg: &Config) -> Self {
        Self {
            api_key: cli.api_key.clone(),
            custom_providers: cfg.custom_providers_map(),
            api_keys: cfg.api_keys.clone(),
        }
    }
}

fn cache_insert(provider: &str, models: Arc<[ModelEntry]>) {
    MODEL_CACHE
        .lock()
        .unwrap()
        .insert(provider.to_string(), models);
    LISTING_FAILURES.lock().unwrap().remove(provider);
    MODEL_CACHE_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Whether a listing for `provider` failed within the TTL (as of `now`).
fn recently_failed(provider: &str, now: Instant) -> bool {
    LISTING_FAILURES
        .lock()
        .unwrap()
        .get(provider)
        .is_some_and(|failed| now.saturating_duration_since(*failed) < LISTING_FAILURE_TTL)
}

/// The cached listing or the baked catalog, without any network access.
fn cached_or_baked(provider: &str, is_custom: bool) -> Option<Arc<[ModelEntry]>> {
    if let Some(hit) = MODEL_CACHE.lock().unwrap().get(provider) {
        return Some(Arc::clone(hit)); // guard dropped here, NOT across any await
    }
    // No cache yet: serve the baked catalog for built-in providers — no network.
    let entries = (!is_custom)
        .then(|| crate::models_catalog::catalog_entries(provider))
        .flatten()?;
    let models: Vec<ModelEntry> = entries
        .iter()
        .filter(|m| crate::provider::is_agent_model(m))
        .cloned()
        .collect();
    let arc: Arc<[ModelEntry]> = Arc::from(models.into_boxed_slice());
    cache_insert(provider, Arc::clone(&arc));
    Some(arc)
}

/// Returns the provider's models.
///
/// Network is only touched on `refresh`, for custom gateways, or for built-in
/// providers that aren't baked (e.g. ollama). Baked built-ins are served from
/// the embedded catalog with no network call — this is what keeps startup instant.
/// A recent failure is returned without a new request unless `refresh` is set.
pub(crate) async fn fetch_models_cached(
    provider: &str,
    is_custom: bool,
    client: &AnyClient,
    cli: &Cli,
    cfg: &Config,
    refresh: bool,
) -> anyhow::Result<Arc<[ModelEntry]>> {
    if !refresh {
        if let Some(hit) = cached_or_baked(provider, is_custom) {
            return Ok(hit);
        }
        if recently_failed(provider, Instant::now()) {
            anyhow::bail!(
                "model listing for {provider} failed recently; run /models refresh to retry"
            );
        }
    }
    fetch_and_cache(
        provider,
        is_custom,
        client,
        &ListingCredentials::new(cli, cfg),
    )
    .await
}

async fn fetch_and_cache(
    provider: &str,
    is_custom: bool,
    client: &AnyClient,
    creds: &ListingCredentials,
) -> anyhow::Result<Arc<[ModelEntry]>> {
    let result = fetch_uncached(provider, is_custom, client, creds).await;
    match &result {
        Ok(models) => cache_insert(provider, Arc::clone(models)),
        Err(_) => {
            LISTING_FAILURES
                .lock()
                .unwrap()
                .insert(provider.to_string(), Instant::now());
        }
    }
    result
}

async fn fetch_uncached(
    provider: &str,
    is_custom: bool,
    client: &AnyClient,
    creds: &ListingCredentials,
) -> anyhow::Result<Arc<[ModelEntry]>> {
    let mut models = if is_custom {
        list_models_manual(
            provider,
            creds.api_key.as_deref(),
            &creds.custom_providers,
            creds.api_keys.as_ref(),
        )
        .await?
    } else {
        client.list_models().await?
    };
    models.retain(crate::provider::is_agent_model);

    if provider == "openrouter" {
        match crate::provider::fetch_openrouter_pricing(
            creds.api_key.as_deref(),
            &creds.custom_providers,
            creds.api_keys.as_ref(),
        )
        .await
        {
            Ok(prices) => {
                for m in &mut models {
                    if let Some(info) = prices.get(&m.id) {
                        m.input_price = Some(info.input_cost);
                        m.output_price = Some(info.output_cost);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("failed to fetch OpenRouter pricing: {e}");
            }
        }
    }

    Ok(Arc::from(models.into_boxed_slice()))
}

/// sync read for the picker (no await)
pub(crate) fn cached_model_ids(provider: &str) -> Vec<String> {
    MODEL_CACHE
        .lock()
        .unwrap()
        .get(provider)
        .map(|v| v.iter().map(|m| m.id.clone()).collect())
        .unwrap_or_default()
}

/// The cache generation; changes whenever a listing lands in the cache.
pub(crate) fn model_cache_generation() -> u64 {
    MODEL_CACHE_GENERATION.load(Ordering::Relaxed)
}

/// Make sure `provider`'s models get cached without blocking the caller: a
/// cached or baked list is used as is, a recent failure is not retried, and
/// otherwise one background task per provider fetches the list. Read the
/// result with [`cached_model_ids`] once [`model_cache_generation`] changes.
pub(crate) fn warm_model_cache(
    provider: &str,
    is_custom: bool,
    client: &AnyClient,
    cli: &Cli,
    cfg: &Config,
) {
    if cached_or_baked(provider, is_custom).is_some()
        || recently_failed(provider, Instant::now())
        || !WARMING.lock().unwrap().insert(provider.to_string())
    {
        return;
    }
    // Outside a runtime (never in the app) there is nothing to spawn on.
    if tokio::runtime::Handle::try_current().is_err() {
        WARMING.lock().unwrap().remove(provider);
        return;
    }
    let provider = provider.to_string();
    let client = client.clone();
    let creds = ListingCredentials::new(cli, cfg);
    tokio::spawn(async move {
        if let Err(error) = fetch_and_cache(&provider, is_custom, &client, &creds).await {
            tracing::debug!("background model listing for {provider} failed: {error}");
        }
        WARMING.lock().unwrap().remove(&provider);
    });
}

fn lookup_pricing_from_cache(provider: &str, model_id: &str) -> Option<(f64, f64)> {
    MODEL_CACHE
        .lock()
        .unwrap()
        .get(provider)
        .and_then(|models| {
            models.iter().find_map(|m| {
                if m.id == model_id {
                    m.input_price.zip(m.output_price).or_else(|| {
                        crate::models_catalog::catalog_entries(provider).and_then(|entries| {
                            entries.iter().find_map(|e| {
                                if e.id == model_id {
                                    e.input_price.zip(e.output_price)
                                } else {
                                    None
                                }
                            })
                        })
                    })
                } else {
                    None
                }
            })
        })
}

async fn apply_model(ctx: &mut SlashCtx<'_>, model_id: &str) {
    let new_model = compact_str::CompactString::new(model_id);
    let new_agent = ctx
        .agent_build_ctx()
        .rebuild_agent(&new_model, *ctx.reasoning_enabled)
        .await;
    *ctx.agent = Some(new_agent);
    ctx.session.model = new_model.clone();
    ctx.session
        .update_context_window(ctx.cfg.resolve_context_window(
            &ctx.session.provider,
            &new_model,
            &crate::config::quick_models_map(ctx.cfg),
        ));
    if let Some((input, output)) = lookup_pricing_from_cache(&ctx.session.provider, model_id) {
        ctx.session.input_token_cost = input;
        ctx.session.output_token_cost = output;
    } else if ctx.session.provider == "openrouter"
        && let Ok(prices) = crate::provider::fetch_openrouter_pricing(
            ctx.cli.api_key.as_deref(),
            &ctx.cfg.custom_providers_map(),
            ctx.cfg.api_keys.as_ref(),
        )
        .await
        && let Some(info) = prices.get(model_id)
    {
        ctx.session.input_token_cost = info.input_cost;
        ctx.session.output_token_cost = info.output_cost;
        if ctx.cfg.context_window.is_none()
            && crate::config::Config::catalog_context_window("openrouter", model_id).is_none()
            && let Some(cw) = info.context_length
        {
            ctx.session.update_context_window(cw);
        }
    }
    write_ok(ctx.renderer, format!("switched to model: {}", new_model));
}

async fn handle_provider(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 {
        write_ok(
            ctx.renderer,
            format!("current provider: {}", ctx.session.provider),
        );
        return Ok(());
    }
    let new_provider = parts[1].trim();
    if crate::provider::parse_provider(new_provider).is_none()
        && !ctx.cfg.custom_providers_map().contains_key(new_provider)
    {
        write_error(
            ctx.renderer,
            format!("unknown provider: '{}'", new_provider),
        );
        return Ok(());
    }
    // Create the client first: a failure must not leave the session's model
    // or costs switched to the new provider while the old one stays active.
    ctx.switch_client(new_provider)?;
    // Default the model to something valid for the new provider. Otherwise
    // the old id (e.g. an OpenRouter id) is carried onto a provider where it
    // is invalid.
    if let Some((model, costs)) = crate::provider::default_model_for_provider(new_provider, ctx.cfg)
    {
        ctx.session.model = compact_str::CompactString::new(&model);
        if let Some((inc, outc)) = costs {
            ctx.session.input_token_cost = inc;
            ctx.session.output_token_cost = outc;
        }
    }
    ctx.rebuild_agent().await;
    ctx.session
        .update_context_window(ctx.cfg.resolve_context_window(
            new_provider,
            &ctx.session.model,
            &crate::config::quick_models_map(ctx.cfg),
        ));
    write_ok(
        ctx.renderer,
        format!(
            "switched to provider: {} (model: {})",
            new_provider, ctx.session.model
        ),
    );
    Ok(())
}

/// `/model` is the single model selector: `/model` shows the current model,
/// `/model <name>` switches to a quick-model alias, else to a raw model id on
/// the current provider. `/models <name>` does the same for compatibility.
async fn handle_model(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 {
        write_ok(
            ctx.renderer,
            format!(
                "current model: {} ({})",
                ctx.session.model, ctx.session.provider
            ),
        );
        write_result(
            ctx.renderer,
            "  /model <alias|id> switches (Tab in the picker toggles quick aliases and provider models); /models lists them",
        );
        return Ok(());
    }
    switch_model(ctx, parts[1].trim()).await
}

/// How `/model <arg>` resolves its argument.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ModelTarget<'a> {
    /// A `[quick_models]` alias (it may also switch provider).
    Quick(&'a str),
    /// A raw model id on the current provider.
    Raw(&'a str),
}

/// Aliases win over raw ids, so a quick model named like a model id selects
/// the alias's provider and pricing.
pub(crate) fn resolve_model_target<'a>(
    arg: &'a str,
    quick: &HashMap<String, config::QuickModelConfig>,
) -> ModelTarget<'a> {
    if quick.contains_key(arg) {
        ModelTarget::Quick(arg)
    } else {
        ModelTarget::Raw(arg)
    }
}

async fn switch_model(ctx: &mut SlashCtx<'_>, arg: &str) -> anyhow::Result<()> {
    let qm = config::quick_models_map(ctx.cfg);
    match resolve_model_target(arg, &qm) {
        ModelTarget::Quick(name) => {
            let q = &qm[name];
            ctx.switch_client(&q.provider)?;
            apply_model(ctx, &q.model).await;
            // preserve v1.4.x pricing/cost tracking
            ctx.session.input_token_cost = q.input_token_cost;
            ctx.session.output_token_cost = q.output_token_cost;
            write_result(
                ctx.renderer,
                format!(
                    "  quick model {} — ${:.4}/M in  ${:.4}/M out",
                    name, q.input_token_cost, q.output_token_cost
                ),
            );
        }
        ModelTarget::Raw(id) => apply_model(ctx, id).await,
    }
    Ok(())
}

async fn handle_models(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let qm = config::quick_models_map(ctx.cfg);
    let provider = ctx.session.provider.to_string();
    let is_custom = ctx.cfg.custom_providers_map().contains_key(&provider);

    let refresh = parts.get(1).map(|s| s.trim()) == Some("refresh");

    // /models <name-or-id> — same as /model <name-or-id>.
    if parts.len() >= 2 && !refresh {
        return switch_model(ctx, parts[1].trim()).await;
    }

    // ---- list mode (+ optional refresh) ----
    match fetch_models_cached(&provider, is_custom, ctx.client, ctx.cli, ctx.cfg, refresh).await {
        Ok(models) => {
            ctx.input.set_live_model_names(cached_model_ids(&provider));
            if refresh {
                // Explicit refresh: just confirm with a count overview — the picker
                // already holds the full list, so don't dump it to the scrollback.
                // Dim (DarkGrey), matching the "[system] loaded AGENTS.md" startup notices.
                write_result(
                    ctx.renderer,
                    format!(
                        "model list refreshed — quick models: {}, {} models: {}",
                        qm.len(),
                        provider,
                        models.len()
                    ),
                );
            } else {
                // Full listing: quick models, then the provider's available models.
                let mut sorted: Vec<&String> = qm.keys().collect();
                sorted.sort();
                write_ok(
                    ctx.renderer,
                    format!(
                        "quick models (current: {} | {}):",
                        ctx.session.provider, ctx.session.model
                    ),
                );
                if sorted.is_empty() {
                    write_result(ctx.renderer, "  (none — add with /models-add)");
                }
                for name in &sorted {
                    let q = &qm[name.as_str()];
                    write_result(
                        ctx.renderer,
                        format!(
                            "  {}  ({} / {})  ${:.4}/M in  ${:.4}/M out",
                            name, q.provider, q.model, q.input_token_cost, q.output_token_cost
                        ),
                    );
                }
                if !models.is_empty() {
                    write_ok(
                        ctx.renderer,
                        format!("available from {} ({}):", provider, models.len()),
                    );
                    const CAP: usize = 50;
                    for m in models.iter().take(CAP) {
                        let ctx_win = m
                            .context_length
                            .map(|c| format!("  [{}k ctx]", c / 1000))
                            .unwrap_or_default();
                        let label = if m.display == m.id {
                            m.id.clone()
                        } else {
                            format!("{} ({})", m.display, m.id)
                        };
                        write_result(ctx.renderer, format!("  {}{}", label, ctx_win));
                    }
                    if models.len() > CAP {
                        write_result(
                            ctx.renderer,
                            format!(
                                "  … {} more — type /models <filter> or use the picker",
                                models.len() - CAP
                            ),
                        );
                    }
                }
            }
        }
        Err(e) => {
            tracing::debug!("model listing failed for {}: {}", provider, e);
            if refresh {
                write_error(ctx.renderer, format!("model list refresh failed: {}", e));
            } else if is_custom {
                write_result(
                    ctx.renderer,
                    "  (live model list unavailable; type the model id directly)",
                );
            }
        }
    }
    Ok(())
}

async fn handle_models_add(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    const USAGE: &str =
        "usage: /models-add <name> <provider> <model> [input_cost_per_M output_cost_per_M]";
    let Ok((name, provider, model, input_cost, output_cost)) = parse_models_add(parts) else {
        write_error(ctx.renderer, USAGE);
        return Ok(());
    };
    match config::save_quick_model(name, provider, model, input_cost, output_cost) {
        Ok(()) => {
            write_ok(
                ctx.renderer,
                format!(
                    "saved quick model: {} ({} / {})  ${}/M in  ${}/M out",
                    name, provider, model, input_cost, output_cost
                ),
            );
        }
        Err(e) => {
            write_error(ctx.renderer, format!("failed to save quick model: {}", e));
        }
    }
    Ok(())
}

fn parse_models_add<'a>(parts: &'a [&'a str]) -> Result<(&'a str, &'a str, &'a str, f64, f64), ()> {
    if !matches!(parts.len(), 4 | 6) || parts.first() != Some(&"/models-add") {
        return Err(());
    }
    let (input_cost, output_cost) = if parts.len() == 6 {
        let input = parts[4].parse::<f64>().map_err(|_| ())?;
        let output = parts[5].parse::<f64>().map_err(|_| ())?;
        if !input.is_finite() || !output.is_finite() || input < 0.0 || output < 0.0 {
            return Err(());
        }
        (input, output)
    } else {
        (0.0, 0.0)
    };
    Ok((parts[1], parts[2], parts[3], input_cost, output_cost))
}

#[cfg(feature = "subagents")]
async fn handle_model_subagent(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    use crate::extras::subagents;

    if parts.len() < 2 {
        let (provider_name, model_name) =
            subagents::with_config(|cfg| (cfg.provider_name.clone(), cfg.model_name.clone()))?;
        write_ok(
            ctx.renderer,
            format!("current subagent model: {} / {}", provider_name, model_name),
        );
        return Ok(());
    }

    let new_model = parts[1].trim().to_string();
    let subagent_client =
        subagents::with_config(|cfg| cfg.client.clone()).map_err(anyhow::Error::from)?;
    let model = subagent_client.completion_model(new_model.clone());
    model_for_subagent(ctx, model).await?;
    subagents::set_model_name(new_model.clone());
    write_ok(
        ctx.renderer,
        format!("switched subagent to model: {}", new_model),
    );
    Ok(())
}

#[cfg(feature = "subagents")]
async fn handle_models_subagent(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    use crate::extras::subagents;

    let qm = config::quick_models_map(ctx.cfg);
    let mut sorted: Vec<&String> = qm.keys().collect();
    sorted.sort();

    if parts.len() < 2 {
        let (provider_name, model_name) =
            subagents::with_config(|cfg| (cfg.provider_name.clone(), cfg.model_name.clone()))?;
        if sorted.is_empty() {
            write_ok(
                ctx.renderer,
                format!(
                    "current subagent: {} / {} (no quick models defined)",
                    provider_name, model_name
                ),
            );
        } else {
            write_ok(
                ctx.renderer,
                format!(
                    "quick models (current subagent: {} | {}):",
                    provider_name, model_name
                ),
            );
            for name in &sorted {
                let q = &qm[name.as_str()];
                write_result(
                    ctx.renderer,
                    format!(
                        "  {}  ({} / {})  ${:.4}/M in  ${:.4}/M out",
                        name, q.provider, q.model, q.input_token_cost, q.output_token_cost
                    ),
                );
            }
        }
        return Ok(());
    }

    let name = parts[1].trim();
    if let Some(q) = qm.get(name) {
        let (current_provider, current_client) =
            subagents::with_config(|cfg| (cfg.provider_name.clone(), cfg.client.clone()))?;
        if q.provider.as_str() != current_provider {
            let new_client = crate::provider::create_client(
                &q.provider,
                ctx.cli.api_key.as_deref(),
                &ctx.cfg.custom_providers_map(),
                ctx.cfg.api_keys.as_ref(),
            )?;
            let model = new_client.completion_model(q.model.to_string());
            model_for_subagent(ctx, model).await?;
            subagents::set_client_and_model(
                new_client,
                q.provider.to_string(),
                q.model.to_string(),
            );
        } else {
            let model = current_client.completion_model(q.model.to_string());
            model_for_subagent(ctx, model).await?;
            subagents::set_model_name(q.model.to_string());
        }
        write_ok(
            ctx.renderer,
            format!(
                "switched subagent to quick model: {} ({} / {})  ${:.4}/M in  ${:.4}/M out",
                name, q.provider, q.model, q.input_token_cost, q.output_token_cost
            ),
        );
    } else {
        write_error(ctx.renderer, format!("unknown quick model: '{}'", name));
        if !sorted.is_empty() {
            write_ok(ctx.renderer, "available quick models:");
            for n in &sorted {
                write_result(ctx.renderer, format!("  {}", n));
            }
        }
    }
    Ok(())
}

/// Validate a model handle by trying to build a subagent with it.
/// If it fails, the error is shown but does not abort the command.
#[cfg(feature = "subagents")]
async fn model_for_subagent(
    ctx: &mut SlashCtx<'_>,
    model: crate::provider::AnyModel,
) -> anyhow::Result<()> {
    let max_turns = ctx.cfg.task_max_turns.unwrap_or(20);
    let _agent = crate::extras::subagents::builder::build_explore_agent(
        model,
        max_turns,
        ctx.cfg,
        crate::extras::subagents::builder::SubagentAuthorization::new(
            ctx.permission.clone(),
            ctx.ask_tx.clone(),
            ctx.cfg.deny_repeated_reads.unwrap_or(true),
        ),
        #[cfg(feature = "archmd")]
        None,
        None,
        crate::extras::subagents::builder::PersonaExecution::default(),
        #[cfg(feature = "skills")]
        None,
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod subagent_command_tests {
    use super::{SUBAGENTS_DISABLED, SubagentModelCommand, subagent_model_command};

    #[test]
    fn subagent_model_commands_are_recognised_in_every_build() {
        assert_eq!(
            subagent_model_command("/subagent-model"),
            Some(SubagentModelCommand::Model)
        );
        assert_eq!(
            subagent_model_command("/subagent-models"),
            Some(SubagentModelCommand::Models)
        );
        assert_eq!(
            subagent_model_command("/model-subagent"),
            Some(SubagentModelCommand::Model)
        );
        assert_eq!(
            subagent_model_command("/models-subagent"),
            Some(SubagentModelCommand::Models)
        );
        assert_eq!(subagent_model_command("/model"), None);
        assert!(crate::ui::slash::routes_to_providers("/subagent-model"));
        assert!(crate::ui::slash::routes_to_providers("/subagent-models"));
        assert!(crate::ui::slash::routes_to_providers("/model-subagent"));
        assert!(crate::ui::slash::routes_to_providers("/models-subagent"));
    }

    #[test]
    fn completion_offers_only_the_canonical_names() {
        let offered = crate::ui::pickers::list::available_commands();
        assert!(!offered.contains(&"/model-subagent"));
        assert!(!offered.contains(&"/models-subagent"));
        #[cfg(feature = "subagents")]
        {
            assert!(offered.contains(&"/subagent-model"));
            assert!(offered.contains(&"/subagent-models"));
        }
    }

    #[test]
    fn disabled_message_names_the_feature() {
        assert!(SUBAGENTS_DISABLED.contains("'subagents' feature"));
    }
}

/// mini-agent-bgg91: failed listings are cached so slash commands never wait
/// on an unreachable gateway again within the TTL, and warming never blocks.
#[cfg(test)]
mod model_cache_tests {
    use super::*;
    use clap::Parser;

    fn fixtures() -> (AnyClient, Cli, Config) {
        let client = AnyClient::OpenRouter(
            rig::providers::openrouter::Client::builder()
                .api_key("unused-test-key")
                .build()
                .unwrap(),
        );
        (
            client,
            Cli::parse_from(["mini-agent", "--api-key", "unused-test-key"]),
            Config::default(),
        )
    }

    #[tokio::test]
    async fn a_failed_listing_is_not_retried_until_refresh_or_ttl() {
        let (client, cli, cfg) = fixtures();
        let provider = format!("missing-gateway-{}", uuid::Uuid::new_v4());
        let first = fetch_models_cached(&provider, true, &client, &cli, &cfg, false)
            .await
            .err()
            .expect("listing must fail");
        assert!(!first.to_string().contains("failed recently"), "{first}");
        assert!(recently_failed(&provider, Instant::now()));

        let cached = fetch_models_cached(&provider, true, &client, &cli, &cfg, false)
            .await
            .err()
            .expect("listing must fail");
        assert!(cached.to_string().contains("failed recently"), "{cached}");

        let refreshed = fetch_models_cached(&provider, true, &client, &cli, &cfg, true)
            .await
            .err()
            .expect("listing must fail");
        assert!(!refreshed.to_string().contains("failed recently"));

        let later = Instant::now() + LISTING_FAILURE_TTL + Duration::from_secs(1);
        assert!(!recently_failed(&provider, later), "the TTL expires");
    }

    #[tokio::test]
    async fn warming_returns_immediately_and_skips_known_failures() {
        let (client, cli, cfg) = fixtures();
        let provider = format!("missing-gateway-{}", uuid::Uuid::new_v4());
        LISTING_FAILURES
            .lock()
            .unwrap()
            .insert(provider.clone(), Instant::now());
        warm_model_cache(&provider, true, &client, &cli, &cfg);
        assert!(!WARMING.lock().unwrap().contains(&provider));

        // A baked built-in catalog warms synchronously, with no task.
        let before = model_cache_generation();
        warm_model_cache("anthropic", false, &client, &cli, &cfg);
        if crate::models_catalog::catalog_entries("anthropic").is_some() {
            assert!(!cached_model_ids("anthropic").is_empty());
            assert!(model_cache_generation() >= before);
        }
        assert!(!WARMING.lock().unwrap().contains("anthropic"));
    }
}

#[cfg(test)]
mod model_target_tests {
    use super::{ModelTarget, resolve_model_target};
    use crate::config::QuickModelConfig;
    use std::collections::HashMap;

    /// mini-agent-9rbwc: `/model` resolves a quick alias before a raw id.
    #[test]
    fn aliases_resolve_before_raw_ids() {
        let quick: HashMap<String, QuickModelConfig> = serde_json::from_value(serde_json::json!({
            "fast": {"provider": "openrouter", "model": "org/fast-model"}
        }))
        .unwrap();
        assert_eq!(
            resolve_model_target("fast", &quick),
            ModelTarget::Quick("fast")
        );
        assert_eq!(
            resolve_model_target("org/fast-model", &quick),
            ModelTarget::Raw("org/fast-model")
        );
        assert_eq!(
            resolve_model_target("fast", &HashMap::new()),
            ModelTarget::Raw("fast")
        );
    }
}

#[cfg(test)]
mod models_add_tests {
    use super::parse_models_add;

    #[test]
    fn parses_optional_cost_pair_without_corrupting_model_id() {
        assert_eq!(
            parse_models_add(&[
                "/models-add",
                "fast",
                "openrouter",
                "org/model",
                "1.5",
                "2.75"
            ]),
            Ok(("fast", "openrouter", "org/model", 1.5, 2.75))
        );
    }

    #[test]
    fn rejects_partial_invalid_and_non_finite_costs() {
        assert!(parse_models_add(&["/models-add", "fast", "p", "m", "1.0"]).is_err());
        assert!(parse_models_add(&["/models-add", "fast", "p", "m", "x", "2.0"]).is_err());
        assert!(parse_models_add(&["/models-add", "fast", "p", "m", "NaN", "2.0"]).is_err());
        assert!(parse_models_add(&["/models-add", "fast", "p", "m", "-1", "2.0"]).is_err());
    }
}
