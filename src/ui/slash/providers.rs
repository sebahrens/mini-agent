use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
use crate::cli::Cli;
use crate::config::{self, Config};
use crate::provider::{AnyClient, ModelEntry};
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

use crate::provider_listing::ListingKey;
struct CachedListing {
    models: Arc<[ModelEntry]>,
    bytes: usize,
}
static MODEL_CACHE: LazyLock<Mutex<HashMap<ListingKey, CachedListing>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static MODEL_CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);
static LISTING_FAILURES: LazyLock<Mutex<HashMap<ListingKey, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static WARMING: LazyLock<Mutex<HashSet<ListingKey>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static TICKETS: LazyLock<Mutex<HashMap<ListingKey, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);
const LISTING_FAILURE_TTL: Duration = Duration::from_secs(300);
const CACHE_BYTES: usize = 32 * 1024 * 1024;

fn cache_insert(key: &ListingKey, ticket: u64, models: Arc<[ModelEntry]>) -> anyhow::Result<()> {
    cache_insert_with_limit(key, ticket, models, CACHE_BYTES)
}
fn cache_insert_with_limit(
    key: &ListingKey,
    ticket: u64,
    models: Arc<[ModelEntry]>,
    limit: usize,
) -> anyhow::Result<()> {
    let tickets = TICKETS.lock().unwrap();
    anyhow::ensure!(
        tickets.get(key) == Some(&ticket),
        "model listing superseded"
    );
    let bytes = models
        .len()
        .saturating_mul(std::mem::size_of::<ModelEntry>())
        .saturating_add(
            models
                .iter()
                .map(|m| {
                    m.id.capacity()
                        + m.display.capacity()
                        + m.kind.as_ref().map_or(0, String::capacity)
                })
                .sum::<usize>(),
        );
    let mut cache = MODEL_CACHE.lock().unwrap();
    cache.retain(|key, _| key.is_live());
    let used = cache
        .iter()
        .filter(|(other, _)| *other != key)
        .map(|(_, v)| v.bytes)
        .sum::<usize>();
    anyhow::ensure!(
        bytes <= limit.saturating_sub(used),
        "model listing cache capacity exceeded"
    );
    cache.insert(key.clone(), CachedListing { models, bytes });
    LISTING_FAILURES.lock().unwrap().remove(key);
    MODEL_CACHE_GENERATION.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
fn recently_failed(key: &ListingKey, now: Instant) -> bool {
    LISTING_FAILURES
        .lock()
        .unwrap()
        .get(key)
        .is_some_and(|failed| now.saturating_duration_since(*failed) < LISTING_FAILURE_TTL)
}
fn cached_or_baked(provider: &str, is_custom: bool, key: &ListingKey) -> Option<Arc<[ModelEntry]>> {
    if let Some(hit) = MODEL_CACHE.lock().unwrap().get(key) {
        return Some(hit.models.clone());
    }
    let entries = (!is_custom)
        .then(|| crate::models_catalog::catalog_entries(provider))
        .flatten()?;
    Some(Arc::from(
        entries
            .iter()
            .filter(|m| crate::provider::is_agent_model(m))
            .cloned()
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    ))
}
pub(crate) async fn fetch_models_cached(
    provider: &str,
    is_custom: bool,
    client: &AnyClient,
    refresh: bool,
) -> anyhow::Result<Arc<[ModelEntry]>> {
    let key = client.listing_key(provider)?;
    if !refresh {
        if let Some(hit) = cached_or_baked(provider, is_custom, &key) {
            return Ok(hit);
        }
        if recently_failed(&key, Instant::now()) {
            anyhow::bail!("model listing failed recently; run /models refresh to retry");
        }
    }
    fetch_and_cache(provider, client, key).await
}
async fn fetch_and_cache(
    provider: &str,
    client: &AnyClient,
    key: ListingKey,
) -> anyhow::Result<Arc<[ModelEntry]>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let ticket = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
    {
        let mut tickets = TICKETS.lock().unwrap();
        tickets.retain(|key, _| key.is_live());
        anyhow::ensure!(
            tickets.contains_key(&key) || tickets.len() < 64,
            "model listing cache capacity exceeded"
        );
        tickets.insert(key.clone(), ticket);
    }
    let result = async {
        let mut models = client.list_models().await?;
        if !key.has_live_prices() {
            for model in &mut models {
                if let Some(baked) = crate::models_catalog::catalog_entries(provider)
                    .and_then(|v| v.iter().find(|m| m.id == model.id))
                {
                    model.context_length = model.context_length.or(baked.context_length);
                    model.input_price = baked.input_price;
                    model.output_price = baked.output_price;
                    model.cache_read_price = baked.cache_read_price;
                    model.cache_write_price = baked.cache_write_price;
                }
            }
        }
        let models: Arc<[ModelEntry]> = Arc::from(models.into_boxed_slice());
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "model listing deadline exceeded"
        );
        cache_insert(&key, ticket, models.clone())?;
        Ok(models)
    }
    .await;
    if result.is_err() {
        // A stale failure cannot suppress the latest successful route revision.
        if TICKETS.lock().unwrap().get(&key) == Some(&ticket) {
            let mut failures = LISTING_FAILURES.lock().unwrap();
            failures.retain(|key, _| key.is_live());
            failures.insert(key, Instant::now());
        }
    }
    result
}
pub(crate) fn cached_model_ids(provider: &str, is_custom: bool, client: &AnyClient) -> Vec<String> {
    client
        .listing_key(provider)
        .ok()
        .and_then(|key| cached_or_baked(provider, is_custom, &key))
        .map(|v| v.iter().map(|m| m.id.clone()).collect())
        .unwrap_or_default()
}
pub(crate) fn model_cache_generation() -> u64 {
    MODEL_CACHE_GENERATION.load(Ordering::Relaxed)
}
struct Warming(ListingKey);
impl Drop for Warming {
    fn drop(&mut self) {
        WARMING.lock().unwrap().remove(&self.0);
    }
}
pub(crate) fn warm_model_cache(provider: &str, is_custom: bool, client: &AnyClient) {
    let Ok(key) = client.listing_key(provider) else {
        return;
    };
    if cached_or_baked(provider, is_custom, &key).is_some() || recently_failed(&key, Instant::now())
    {
        return;
    }
    let mut warming = WARMING.lock().unwrap();
    warming.retain(ListingKey::is_live);
    if !warming.insert(key.clone()) {
        return;
    }
    drop(warming);
    let lease = Warming(key.clone());
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let provider = provider.to_owned();
    let client = client.clone();
    tokio::spawn(async move {
        let _lease = lease;
        if let Err(error) = fetch_and_cache(&provider, &client, key).await {
            tracing::debug!("background model listing failed: {error}");
        }
    });
}

/// One pricing order for raw ids at startup and during a model switch. A stale
/// live listing must not hide baked prices, and a previous model is never a
/// pricing fallback for an unknown id.
pub(crate) fn resolve_model_pricing(
    provider: &str,
    model_id: &str,
    cfg: &Config,
    source: Option<&ListingKey>,
) -> Option<(f64, f64)> {
    diagnostic_model_pricing(provider, model_id, cfg, source).0
}

#[cfg(test)]
fn resolve_model_pricing_from_listing(
    provider: &str,
    model_id: &str,
    cfg: &Config,
    listing: Option<&[ModelEntry]>,
) -> Option<(f64, f64)> {
    resolve_model_pricing_with_source_from_listing(provider, model_id, cfg, listing).0
}

pub(crate) fn diagnostic_model_pricing(
    provider: &str,
    model_id: &str,
    cfg: &Config,
    source: Option<&ListingKey>,
) -> (Option<(f64, f64)>, &'static str) {
    let cache = MODEL_CACHE.lock().unwrap();
    let listing = source
        .filter(|key| key.is_live() && key.matches_provider(provider) && key.has_live_prices())
        .and_then(|key| cache.get(key))
        .map(|v| v.models.as_ref());
    let (prices, provenance) =
        resolve_model_pricing_with_source_from_listing(provider, model_id, cfg, listing);
    (
        prices,
        if source.is_none() && provenance == "embedded catalogue" {
            "embedded catalogue (no captured listing route)"
        } else {
            provenance
        },
    )
}

fn resolve_model_pricing_with_source_from_listing(
    provider: &str,
    model_id: &str,
    cfg: &Config,
    listing: Option<&[ModelEntry]>,
) -> (Option<(f64, f64)>, &'static str) {
    let live = listing
        .and_then(|models| models.iter().find(|model| model.id == model_id))
        .and_then(|model| model.input_price.zip(model.output_price));
    if let Some(prices) = live {
        return (Some(prices), "cached live listing");
    }
    if let Some(prices) = Config::catalog_input_output_cost(provider, model_id) {
        return (Some(prices), "embedded catalogue");
    }
    let quick = {
        config::quick_models_map(cfg)
            .into_iter()
            .filter(|(_, quick)| quick.provider == provider && quick.model == model_id)
            // Multiple aliases for one raw id resolve deterministically.
            .min_by(|(a, _), (b, _)| a.cmp(b))
            .and_then(|(_, quick)| quick.resolved_prices())
    };
    (
        quick,
        if quick.is_some() {
            "configured alias fallback"
        } else {
            "unknown"
        },
    )
}

fn apply_model_pricing(
    session: &mut crate::session::Session,
    model_id: &str,
    pricing: Option<(f64, f64)>,
) -> Option<String> {
    session.set_input_output_prices(pricing);
    pricing.is_none().then(|| format!(
        "no pricing known for {model_id}; costs will not be tracked (set input_token_cost/output_token_cost in a quick model)"
    ))
}

fn resolve_selected_model_capacity(
    provider: &str,
    model: &str,
    cfg: &Config,
    selected: Option<&str>,
    source: Option<&ListingKey>,
) -> u64 {
    if let Some(pin) = cfg.context_window {
        return pin;
    }
    let quick = config::quick_models_map(cfg);
    if let Some(pin) = selected
        .and_then(|name| quick.get(name))
        .filter(|q| q.provider == provider && q.model == model)
        .and_then(|q| q.context_window)
    {
        return pin;
    }
    if let Some(pin) = quick
        .iter()
        .filter(|(_, q)| q.provider == provider && q.model == model && q.context_window.is_some())
        .min_by(|(a, _), (b, _)| a.cmp(b))
        .and_then(|(_, q)| q.context_window)
    {
        return pin;
    }
    let live = source
        .filter(|key| key.is_live() && key.matches_provider(provider))
        .and_then(|key| {
            MODEL_CACHE.lock().unwrap().get(key).and_then(|entry| {
                entry
                    .models
                    .iter()
                    .find(|m| m.id == model)
                    .and_then(|m| m.context_length)
            })
        });
    live.map(u64::from)
        .or_else(|| Config::catalog_context_window(provider, model))
        .unwrap_or(128_000)
}

async fn apply_model(ctx: &mut SlashCtx<'_>, model_id: &str, selected_alias: Option<&str>) {
    let new_model = compact_str::CompactString::new(model_id);
    let new_agent = ctx
        .agent_build_ctx()
        .rebuild_agent(&new_model, *ctx.reasoning_enabled)
        .await;
    *ctx.agent = Some(new_agent);
    ctx.session.model = new_model.clone();
    let key = ctx.client.listing_key(&ctx.session.provider).ok();
    let mut pricing = resolve_model_pricing(
        &ctx.session.provider,
        model_id,
        ctx.cfg,
        ctx.client.listing_key(&ctx.session.provider).ok().as_ref(),
    );
    if pricing.is_none()
        && ctx.session.provider == "openrouter"
        && let Ok(models) = fetch_models_cached(
            &ctx.session.provider,
            ctx.cfg
                .custom_providers_map()
                .contains_key(ctx.session.provider.as_str()),
            ctx.client,
            true,
        )
        .await
        && let Some(info) = models.iter().find(|m| m.id == model_id)
    {
        pricing = info.input_price.zip(info.output_price);
    }
    ctx.session
        .update_context_window(resolve_selected_model_capacity(
            &ctx.session.provider,
            model_id,
            ctx.cfg,
            selected_alias,
            key.as_ref(),
        ));
    if let Some(note) = apply_model_pricing(ctx.session, model_id, pricing) {
        let _ = ctx
            .renderer
            .write_line(&note, crossterm::style::Color::Yellow);
    }
    ctx.session
        .set_cache_prices(crate::pricing::cache_prices_for_model(
            &ctx.session.provider,
            model_id,
            ctx.cfg,
            None,
            ctx.session.input_token_cost,
        ));

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
    let mut pricing = None;
    let mut selected_alias = None;
    if let Some((model, costs)) = crate::provider::default_model_for_provider(new_provider, ctx.cfg)
    {
        ctx.session.model = compact_str::CompactString::new(&model);
        if costs.is_some() {
            selected_alias = provider_default_cache_alias(new_provider, &model, ctx.cfg);
        }
        pricing = costs;
    }
    let model = ctx.session.model.to_string();
    let pricing = pricing.or_else(|| {
        resolve_model_pricing(
            new_provider,
            &model,
            ctx.cfg,
            ctx.client.listing_key(new_provider).ok().as_ref(),
        )
    });
    if let Some(note) = apply_model_pricing(ctx.session, &model, pricing) {
        let _ = ctx
            .renderer
            .write_line(&note, crossterm::style::Color::Yellow);
    }
    ctx.session
        .set_cache_prices(crate::pricing::cache_prices_for_model(
            new_provider,
            &model,
            ctx.cfg,
            selected_alias,
            ctx.session.input_token_cost,
        ));
    ctx.rebuild_agent().await;
    ctx.session
        .update_context_window(resolve_selected_model_capacity(
            new_provider,
            &model,
            ctx.cfg,
            selected_alias,
            ctx.client.listing_key(new_provider).ok().as_ref(),
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

fn provider_default_cache_alias<'a>(
    provider: &str,
    model: &str,
    cfg: &'a Config,
) -> Option<&'a str> {
    let quick = cfg.quick_models.as_ref()?;
    let matches =
        |entry: &config::QuickModelConfig| entry.provider == provider && entry.model == model;
    if let Some(alias) = cfg.model.as_deref()
        && quick.get(alias).is_some_and(matches)
    {
        return Some(alias);
    }
    if quick.get("deepseek-v4-pro").is_some_and(matches) {
        return Some("deepseek-v4-pro");
    }
    quick
        .iter()
        .filter(|(_, entry)| matches(entry))
        .min_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(alias, _)| alias.as_str())
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

fn quick_price_notice(quick: &config::QuickModelConfig) -> String {
    quick.resolved_prices().map_or_else(
        || "pricing unknown".to_owned(),
        |(input, output)| format!("${input:.4}/M in  ${output:.4}/M out"),
    )
}

async fn switch_model(ctx: &mut SlashCtx<'_>, arg: &str) -> anyhow::Result<()> {
    let qm = config::quick_models_map(ctx.cfg);
    match resolve_model_target(arg, &qm) {
        ModelTarget::Quick(name) => {
            let q = &qm[name];
            ctx.switch_client(&q.provider)?;
            apply_model(ctx, &q.model, Some(name)).await;
            // preserve v1.4.x pricing/cost tracking
            let input_price = q.resolved_prices().map(|prices| prices.0).unwrap_or(0.0);
            ctx.session.set_input_output_prices(q.resolved_prices());
            ctx.session
                .set_cache_prices(crate::pricing::cache_prices_for_model(
                    &q.provider,
                    &q.model,
                    ctx.cfg,
                    Some(name),
                    input_price,
                ));
            write_result(
                ctx.renderer,
                format!("  quick model {} — {}", name, quick_price_notice(q)),
            );
        }
        ModelTarget::Raw(id) => apply_model(ctx, id, None).await,
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
    match fetch_models_cached(&provider, is_custom, ctx.client, refresh).await {
        Ok(models) => {
            ctx.input
                .set_live_model_names(cached_model_ids(&provider, is_custom, ctx.client));
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
                            "  {}  ({} / {})  {}",
                            name,
                            q.provider,
                            q.model,
                            quick_price_notice(q)
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
                        write_result(
                            ctx.renderer,
                            format!(
                                "  {}{}{}",
                                label,
                                ctx_win,
                                if resolve_model_pricing(
                                    &provider,
                                    &m.id,
                                    ctx.cfg,
                                    ctx.client.listing_key(&provider).ok().as_ref()
                                )
                                .is_none()
                                {
                                    "  (no price known)"
                                } else {
                                    ""
                                }
                            ),
                        );
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
                format!("saved quick model: {} ({} / {})", name, provider, model),
            );
        }
        Err(e) => {
            write_error(ctx.renderer, format!("failed to save quick model: {}", e));
        }
    }
    Ok(())
}

type ModelAddArgs<'a> = (&'a str, &'a str, &'a str, Option<f64>, Option<f64>);

fn parse_models_add<'a>(parts: &'a [&'a str]) -> Result<ModelAddArgs<'a>, ()> {
    if !matches!(parts.len(), 4 | 6) || parts.first() != Some(&"/models-add") {
        return Err(());
    }
    let (input_cost, output_cost) = if parts.len() == 6 {
        let input = parts[4].parse::<f64>().map_err(|_| ())?;
        let output = parts[5].parse::<f64>().map_err(|_| ())?;
        if !input.is_finite() || !output.is_finite() || input < 0.0 || output < 0.0 {
            return Err(());
        }
        (Some(input), Some(output))
    } else {
        (None, None)
    };
    Ok((parts[1], parts[2], parts[3], input_cost, output_cost))
}

#[cfg(feature = "subagents")]
async fn handle_model_subagent(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    use crate::extras::subagents;

    if parts.len() < 2 {
        let status = subagents::with_config(|cfg| cfg.model_status())?;
        write_ok(ctx.renderer, crate::ui::events::sanitize_output(&status));
        return Ok(());
    }

    let new_model = parts[1].trim().to_string();
    let subagent_client =
        subagents::with_config(|cfg| cfg.client.clone()).map_err(anyhow::Error::from)?;
    let model = subagent_client
        .completion_model_with_cache_ttl(new_model.clone(), ctx.cfg.cache_ttl.unwrap_or_default());
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
                        "  {}  ({} / {})  {}",
                        name,
                        q.provider,
                        q.model,
                        quick_price_notice(q)
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
            let model = new_client.completion_model_with_cache_ttl(
                q.model.to_string(),
                ctx.cfg.cache_ttl.unwrap_or_default(),
            );
            model_for_subagent(ctx, model).await?;
            subagents::set_client_and_model(
                new_client,
                q.provider.to_string(),
                q.model.to_string(),
            );
        } else {
            let model = current_client.completion_model_with_cache_ttl(
                q.model.to_string(),
                ctx.cfg.cache_ttl.unwrap_or_default(),
            );
            model_for_subagent(ctx, model).await?;
            subagents::set_model_name(q.model.to_string());
        }
        write_ok(
            ctx.renderer,
            format!(
                "switched subagent to quick model: {} ({} / {})  {}",
                name,
                q.provider,
                q.model,
                quick_price_notice(q)
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn fixtures(address: std::net::SocketAddr) -> AnyClient {
        crate::provider::create_client(
            "local",
            Some("unused-local-key"),
            &HashMap::from([(
                "local".into(),
                serde_json::from_value(serde_json::json!({
                    "provider_type":"openai", "base_url":format!("http://{address}/v1")
                }))
                .unwrap(),
            )]),
            None,
        )
        .unwrap()
    }
    #[tokio::test]
    async fn a_failed_listing_is_not_retried_until_refresh_or_ttl() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = fixtures(listener.local_addr().unwrap());
        let provider = format!("local-{}", uuid::Uuid::new_v4());
        let key = client.listing_key(&provider).unwrap();
        let requests = async {
            let first = fetch_models_cached(&provider, true, &client, false)
                .await
                .err()
                .expect("listing must fail");
            assert!(!first.to_string().contains("failed recently"));
            assert!(recently_failed(&key, Instant::now()));
            let cached = fetch_models_cached(&provider, true, &client, false)
                .await
                .err()
                .expect("listing must fail");
            assert!(cached.to_string().contains("failed recently"));
            let refreshed = fetch_models_cached(&provider, true, &client, true)
                .await
                .err()
                .expect("listing must fail");
            assert!(!refreshed.to_string().contains("failed recently"));
            assert!(!recently_failed(
                &key,
                Instant::now() + LISTING_FAILURE_TTL + Duration::from_secs(1)
            ));
        };
        let server = async {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 8192];
                assert!(socket.read(&mut bytes).await.unwrap() > 0);
                socket.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(requests, server);
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn warming_returns_immediately_and_skips_known_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = fixtures(listener.local_addr().unwrap());
        let provider = format!("local-{}", uuid::Uuid::new_v4());
        let key = client.listing_key(&provider).unwrap();
        LISTING_FAILURES
            .lock()
            .unwrap()
            .insert(key.clone(), Instant::now());
        warm_model_cache(&provider, true, &client);
        assert!(!WARMING.lock().unwrap().contains(&key));
        let native = AnyClient::Anthropic(
            rig::providers::anthropic::Client::builder()
                .http_client(crate::provider_http::ProviderHttpClient::default())
                .api_key("unused-local-key")
                .base_url(format!("http://{}", listener.local_addr().unwrap()))
                .build()
                .unwrap(),
        );
        warm_model_cache("anthropic", false, &native);
        assert!(!cached_model_ids("anthropic", false, &native).is_empty());
        assert!(
            !WARMING
                .lock()
                .unwrap()
                .contains(&native.listing_key("anthropic").unwrap())
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
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
    fn quick_model_price_presence_models_add_omitted_rates_stay_absent() {
        assert_eq!(
            parse_models_add(&["/models-add", "paid", "anthropic", "claude-sonnet-5-5"]),
            Ok(("paid", "anthropic", "claude-sonnet-5-5", None, None))
        );
        assert_eq!(
            parse_models_add(&["/models-add", "free", "gateway", "unlisted", "0", "0"]),
            Ok(("free", "gateway", "unlisted", Some(0.0), Some(0.0)))
        );
    }

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
            Ok(("fast", "openrouter", "org/model", Some(1.5), Some(2.75)))
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

#[cfg(test)]
mod model_pricing_tests {
    use super::*;
    use crate::session::Session;

    #[tokio::test]
    async fn model_command_switches_unknown_and_catalog_models_without_stale_costs() {
        use crate::ui::{input::InputEditor, renderer::Renderer, terminal::TerminalGuard};
        let workspace =
            crate::agent::tools::capture_workspace_binding(std::env::current_dir().unwrap());
        let cli = Cli {
            no_tools: true,
            api_key: Some("unused-test-key".into()),
            ..Cli::default()
        };
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "custom_providers": { "unpriced-gateway": {
                "provider_type": "openai", "base_url": "http://127.0.0.1:1/v1", "api_style": "completions"
            } }
        })).unwrap();
        let mut context = crate::context::ContextFiles {
            #[cfg(feature = "mcp")]
            mcp_discovery: Default::default(),
            workspace_root: workspace.root().to_path_buf(),
            agents: None,
            instruction_roots: Vec::new(),
            prompts: Default::default(),
            current_prompt: None,
            current_prompt_name: None,
            agent_definitions: Default::default(),
            current_agent_name: None,
            current_agent_explicit: false,
            themes: Default::default(),
            current_theme_name: None,
            extra_files: Vec::new(),
            extra_file_contents: Default::default(),
            one_shot_restore: None,
            chain_declined: Vec::new(),
            #[cfg(feature = "memory")]
            memory: None,
            #[cfg(feature = "archmd")]
            architecture: None,
        };
        let mut client = AnyClient::Anthropic(
            rig::providers::anthropic::Client::builder()
                .http_client(crate::provider_http::ProviderHttpClient::default())
                .api_key("unused-test-key")
                .build()
                .unwrap(),
        );
        let mut session = Session::new("anthropic", "previous-model", 128_000, "");
        session.input_token_cost = 7.0;
        session.output_token_cost = 19.0;
        let mut agent = None;
        let mut renderer = Renderer::new().unwrap();
        let mut input = InputEditor::new();
        let mut terminal_guard = TerminalGuard::detached_for_test();
        let invalidated = std::sync::atomic::AtomicBool::new(false);
        let mut show_reasoning = false;
        let mut reasoning_enabled = false;
        let mut is_running = false;
        let mut todo_tools_enabled = false;
        let sandbox =
            crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace.clone());
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
            show_reasoning: &mut show_reasoning,
            reasoning_enabled: &mut reasoning_enabled,
            is_running: &mut is_running,
            input: &mut input,
            permission: &None,
            ask_tx: &None,
            todo_tools_enabled: &mut todo_tools_enabled,
            sandbox: &sandbox,
            terminal_guard: &mut terminal_guard,
            #[cfg(feature = "skills")]
            skill_services: &skill_services,
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        handle(&["/model", "unknown-pricing-test-model"], &mut ctx)
            .await
            .unwrap();
        assert!(ctx.agent.is_some());
        assert_eq!(ctx.session.model, "unknown-pricing-test-model");
        assert_eq!(
            (ctx.session.input_token_cost, ctx.session.output_token_cost),
            (0.0, 0.0)
        );
        assert!(
            ctx.renderer
                .feed()
                .block_text(0)
                .unwrap()
                .contains("no pricing known for unknown-pricing-test-model")
        );
        assert!(
            ctx.renderer
                .feed()
                .block_text(1)
                .unwrap()
                .contains("switched to model")
        );
        let model = crate::models_catalog::catalog_entries("anthropic")
            .unwrap()
            .iter()
            .find(|model| model.input_price.zip(model.output_price).is_some())
            .unwrap();
        let expected = model.input_price.zip(model.output_price).unwrap();
        handle(&["/model", &model.id], &mut ctx).await.unwrap();
        assert_eq!(ctx.session.model.as_str(), model.id);
        assert_eq!(
            (ctx.session.input_token_cost, ctx.session.output_token_cost),
            expected
        );
        assert_eq!(
            ctx.renderer.feed().block_count(),
            3,
            "known model emitted a pricing warning"
        );
        assert!(crate::provider::default_model_for_provider("unpriced-gateway", &cfg).is_none());
        handle(&["/provider", "unpriced-gateway"], &mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.session.provider, "unpriced-gateway");
        assert_eq!(
            (ctx.session.input_token_cost, ctx.session.output_token_cost),
            (0.0, 0.0)
        );
        assert!(
            ctx.renderer
                .feed()
                .block_text(3)
                .unwrap()
                .contains("no pricing known for")
        );
        assert!(
            ctx.renderer
                .feed()
                .block_text(4)
                .unwrap()
                .contains("switched to provider: unpriced-gateway")
        );
    }

    #[test]
    fn unknown_model_resets_previous_costs_and_produces_a_warning() {
        let cfg = Config::default();
        let provider = format!("pricing-unknown-{}", uuid::Uuid::new_v4());
        let mut session = Session::new(&provider, "previous-model", 128_000, "");
        session.input_token_cost = 7.0;
        session.output_token_cost = 19.0;
        let pricing = resolve_model_pricing(&provider, "unknown-model", &cfg, None);
        let note = apply_model_pricing(&mut session, "unknown-model", pricing).unwrap();
        assert_eq!(
            (session.input_token_cost, session.output_token_cost),
            (0.0, 0.0)
        );
        assert!(
            note.contains("no pricing known for unknown-model"),
            "{note}"
        );
        assert!(note.contains("costs will not be tracked"), "{note}");
        assert!(
            note.contains("input_token_cost/output_token_cost"),
            "{note}"
        );
    }

    #[test]
    fn catalog_prices_are_available_without_a_matching_live_listing() {
        let model = crate::models_catalog::catalog_entries("anthropic")
            .unwrap()
            .iter()
            .find(|model| model.input_price.zip(model.output_price).is_some())
            .unwrap();
        let expected = model.input_price.zip(model.output_price).unwrap();
        let stale_listing = [ModelEntry {
            id: "stale-unrelated-model".into(),
            display: "stale".into(),
            context_length: None,
            kind: None,
            input_price: Some(99.0),
            output_price: Some(99.0),
            cache_read_price: None,
            cache_write_price: None,
        }];
        let pricing = resolve_model_pricing_from_listing(
            "anthropic",
            &model.id,
            &Config::default(),
            Some(&stale_listing),
        );
        assert_eq!(pricing, Some(expected));
        let mut session = Session::new("anthropic", "previous-model", 128_000, "");
        session.input_token_cost = 99.0;
        session.output_token_cost = 99.0;
        assert!(apply_model_pricing(&mut session, &model.id, pricing).is_none());
        assert_eq!(
            (session.input_token_cost, session.output_token_cost),
            expected
        );
    }

    #[test]
    fn live_prices_precede_quick_model_prices_and_free_models_remain_known() {
        let provider = format!("pricing-live-{}", uuid::Uuid::new_v4());
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "quick_models": { "local": { "provider": provider, "model": "live", "input_token_cost": 9.0, "output_token_cost": 10.0 } }
        })).unwrap();
        let listing = [ModelEntry {
            id: "live".into(),
            display: "live".into(),
            context_length: None,
            kind: None,
            input_price: Some(0.0),
            output_price: Some(0.0),
            cache_read_price: None,
            cache_write_price: None,
        }];
        let pricing = resolve_model_pricing_from_listing(&provider, "live", &cfg, Some(&listing));
        assert_eq!(pricing, Some((0.0, 0.0)));
        let mut session = Session::new(&provider, "previous-model", 128_000, "");
        session.input_token_cost = 99.0;
        session.output_token_cost = 99.0;
        assert!(apply_model_pricing(&mut session, "live", pricing).is_none());
        assert_eq!(
            (session.input_token_cost, session.output_token_cost),
            (0.0, 0.0)
        );
        assert_eq!(
            resolve_model_pricing_from_listing(&provider, "live", &cfg, None),
            Some((9.0, 10.0))
        );
        assert_eq!(
            resolve_model_pricing("other-provider", "live", &cfg, None),
            None
        );
    }
}

#[cfg(test)]
mod model_listing48_cache_tests {
    use super::*;
    fn client(secret: &str, backend: crate::provider_http::ProviderHttpClient) -> AnyClient {
        AnyClient::OpenRouter(
            rig::providers::openrouter::Client::builder()
                .api_key(secret)
                .base_url("http://127.0.0.1:1/v1")
                .http_client(backend)
                .build()
                .unwrap(),
        )
    }
    fn rows(input: f64, context: u32) -> Arc<[ModelEntry]> {
        Arc::from(
            vec![ModelEntry {
                id: "raw-live".into(),
                display: "Raw 雪".into(),
                context_length: Some(context),
                kind: None,
                input_price: Some(input),
                output_price: Some(11.0),
                cache_read_price: None,
                cache_write_price: None,
            }]
            .into_boxed_slice(),
        )
    }
    fn publish(key: &ListingKey, ticket: u64, rows: Arc<[ModelEntry]>) {
        TICKETS.lock().unwrap().insert(key.clone(), ticket);
        cache_insert(key, ticket, rows).unwrap();
    }
    #[test]
    fn model_listing48_clone_identity_stale_publication_and_capacity_preserve_live_prices() {
        let backend = crate::provider_http::ProviderHttpClient::default();
        let first = client("PRIVATE_IDENTITY", backend.clone());
        let same = first.clone();
        let changed_auth = client("other", backend.clone());
        let changed_backend = client("PRIVATE_IDENTITY", Default::default());
        let provider = format!("cache48-{}", uuid::Uuid::new_v4());
        let key = first.listing_key(&provider).unwrap();
        assert_eq!(key, same.listing_key(&provider).unwrap());
        assert_ne!(key, changed_auth.listing_key(&provider).unwrap());
        assert_ne!(key, changed_backend.listing_key(&provider).unwrap());
        assert!(!format!("{key:?} {backend:?}").contains("PRIVATE_IDENTITY"));
        publish(&key, 20, rows(7.0, 64000));
        let cfg = Config::default();
        assert_eq!(
            resolve_model_pricing(
                &provider,
                "raw-live",
                &cfg,
                Some(&same.listing_key(&provider).unwrap())
            ),
            Some((7.0, 11.0))
        );
        assert_eq!(
            resolve_model_pricing(
                &provider,
                "raw-live",
                &cfg,
                Some(&changed_auth.listing_key(&provider).unwrap())
            ),
            None
        );
        assert_eq!(
            resolve_model_pricing(&provider, "raw-live", &cfg, None),
            None
        );
        assert!(cache_insert(&key, 19, rows(99.0, 32000)).is_err());
        assert!(cache_insert_with_limit(&key, 20, rows(99.0, 32000), 0).is_err());
        assert_eq!(
            resolve_model_pricing(&provider, "raw-live", &cfg, Some(&key)),
            Some((7.0, 11.0))
        );
        drop(first);
        drop(same);
        assert!(key.is_live(), "shared configured backend remains retained");
        drop(changed_auth);
        drop(backend);
        assert!(!key.is_live());
    }
    #[test]
    fn model_listing48_capacity_pins_and_offline_sources_are_exact() {
        let native = client("unused", Default::default());
        let key = native.listing_key("openrouter").unwrap();
        publish(&key, 42, rows(7.0, 64000));
        let mut cfg: Config = serde_json::from_value(serde_json::json!({"quick_models":{
            "z":{"provider":"openrouter","model":"raw-live","context_window":90000},
            "a":{"provider":"openrouter","model":"raw-live","context_window":80000},
            "foreign":{"provider":"openai","model":"raw-live","context_window":70000}}}))
        .unwrap();
        assert_eq!(
            resolve_selected_model_capacity("openrouter", "raw-live", &cfg, Some("z"), Some(&key)),
            90000
        );
        assert_eq!(
            resolve_selected_model_capacity("openrouter", "raw-live", &cfg, None, Some(&key)),
            80000
        );
        cfg.context_window = Some(120000);
        assert_eq!(
            resolve_selected_model_capacity("openrouter", "raw-live", &cfg, Some("z"), Some(&key)),
            120000
        );
        assert_eq!(
            resolve_selected_model_capacity(
                "openrouter",
                "raw-live",
                &Config::default(),
                None,
                Some(&key)
            ),
            64000
        );
        assert_eq!(
            resolve_selected_model_capacity(
                "openrouter",
                "raw-live",
                &Config::default(),
                None,
                None
            ),
            128000
        );
        let model = crate::models_catalog::catalog_entries("anthropic")
            .unwrap()
            .iter()
            .find(|m| m.input_price.zip(m.output_price).is_some())
            .unwrap();
        let (price, source) =
            diagnostic_model_pricing("anthropic", &model.id, &Config::default(), None);
        assert_eq!(price, model.input_price.zip(model.output_price));
        assert_eq!(source, "embedded catalogue (no captured listing route)");
    }
    #[cfg(feature = "goal")]
    #[test]
    fn model_listing48_auxiliary_capture_uses_actual_route_and_retains_original_snapshot() {
        let root = crate::tests::OwnedTestDirectory::new("model48-price");
        let account = crate::spend::Account::new("model48", None, 0.0).unwrap();
        let run = crate::spend::run::Run::fixture(
            account.clone(),
            Arc::new(crate::spend::Ledger::new(root.path())),
            "model48",
            crate::spend::PriceSnapshot::capture(Some((2.0, 3.0)), Default::default(), false),
        );
        let native = client("unused", Default::default());
        let key = native.listing_key("openrouter").unwrap();
        publish(&key, 50, rows(7.0, 64000));
        let captured = run.for_model(
            "openrouter",
            "raw-live",
            &Config::default(),
            None,
            Some(&key),
            crate::spend::RequestKind::Subagent,
        );
        assert_eq!(serde_json::to_value(captured.prices).unwrap()["input"], 7.0);
        publish(&key, 51, rows(99.0, 32000));
        assert_eq!(serde_json::to_value(captured.prices).unwrap()["input"], 7.0);
        assert_eq!(serde_json::to_value(run.prices).unwrap()["input"], 2.0);
        assert_eq!(account.snapshot().recorded_spend(), 0.0);
    }
}

#[cfg(test)]
mod model_listing48_cancel_tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    #[tokio::test]
    async fn model_listing48_actual_cancelled_refresh_preserves_previous_complete_price_record() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = AnyClient::OpenRouter(
            rig::providers::openrouter::Client::builder()
                .api_key("unused-local-key")
                .base_url(format!("http://{}", listener.local_addr().unwrap()))
                .http_client(crate::provider_http::ProviderHttpClient::default())
                .build()
                .unwrap(),
        );
        let provider = format!("cancel48-{}", uuid::Uuid::new_v4());
        let key = client.listing_key(&provider).unwrap();
        let models: Arc<[ModelEntry]> = Arc::from(
            vec![ModelEntry {
                id: "before".into(),
                display: "Before".into(),
                context_length: Some(64000),
                kind: None,
                input_price: Some(7.0),
                output_price: Some(11.0),
                cache_read_price: None,
                cache_write_price: None,
            }]
            .into_boxed_slice(),
        );
        TICKETS.lock().unwrap().insert(key.clone(), 100);
        cache_insert(&key, 100, models.clone()).unwrap();
        let mut refresh = Box::pin(fetch_models_cached(&provider, true, &client, true));
        let (mut socket,_)=tokio::time::timeout(Duration::from_secs(2),async{tokio::select!{result=&mut refresh=>panic!("premature refresh: {}",result.is_ok()),peer=listener.accept()=>peer.unwrap()}}).await.unwrap();
        let mut bytes = [0; 8192];
        tokio::time::timeout(Duration::from_secs(2),async{tokio::select!{result=&mut refresh=>panic!("premature refresh: {}",result.is_ok()),received=socket.read(&mut bytes)=>assert!(received.unwrap()>0)}}).await.unwrap();
        drop(refresh);
        assert_eq!(
            resolve_model_pricing(&provider, "before", &Config::default(), Some(&key)),
            Some((7.0, 11.0))
        );
        assert!(Arc::ptr_eq(
            &MODEL_CACHE.lock().unwrap().get(&key).unwrap().models,
            &models
        ));
        drop(socket);
    }
}
