//! Static, embedded model catalog.
//!
//! Model ids change rarely between releases, so instead of hitting each
//! provider's `/models` endpoint at startup (slow — OpenRouter alone returns
//! hundreds of entries and used to block the first frame), we bake a snapshot
//! into the binary. The picker is seeded from this synchronously, with zero
//! network. The live listing is still available on demand via `/models refresh`
//! (see [`crate::ui::slash`]) and for providers not baked here (custom gateways,
//! ollama).
//!
//! The data lives in `data/models.json`, keyed by *mini-agent* provider name
//! (so `gemini`, not models.dev's `google`). Refresh it with
//! `scripts/gen-models-catalog.sh`; `data/models.generated` records when.
//!
//! A newer models.dev snapshot in the user cache (`models-catalog.json`,
//! written by `/models catalog-refresh` or the opt-in
//! `models_catalog_refresh_days` startup refresh) overlays the embedded one:
//! each provider it lists replaces the embedded list, so retired ids drop out.
//! A missing, corrupt or older cache file leaves the embedded snapshot in use.
//! Bedrock and Vertex Claude ids resolve to the native Anthropic entry.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::provider::ModelEntry;

const CATALOG_JSON: &str = include_str!("../data/models.json");
/// RFC 3339 instant the embedded snapshot was generated.
const EMBEDDED_GENERATED_AT: &str = include_str!("../data/models.generated");
/// models.dev source of both the embedded and the refreshed snapshot.
const MODELS_DEV_URL: &str = "https://models.dev/api.json";
/// Upper bound on a fetched models.dev document or a cached overlay.
const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;
/// Cache file name under the cache directory.
const CACHE_FILE: &str = "models-catalog.json";

#[derive(serde::Deserialize, serde::Serialize)]
struct RawModel {
    #[serde(default)]
    image_input: Option<bool>,
    id: String,
    name: String,
    context: Option<u32>,
    /// Provider's maximum output tokens per response (models.dev
    /// `limit.output`).
    #[serde(default)]
    max_output: Option<u32>,
    #[serde(default)]
    input_price: Option<f64>,
    #[serde(default)]
    output_price: Option<f64>,
    #[serde(default)]
    cache_read_price: Option<f64>,
    #[serde(default)]
    cache_write_price: Option<f64>,
    /// Prompt-size premium tier (models.dev `cost.tiers[]` of type context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    long_context: Option<crate::pricing::LongContextPrices>,
}

type RawCatalog = HashMap<String, Vec<RawModel>>;

/// The user-cache overlay document.
#[derive(serde::Deserialize, serde::Serialize)]
struct CatalogOverlay {
    generated_at: String,
    #[serde(default)]
    source: Option<String>,
    providers: RawCatalog,
}

/// Where the catalog in use came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CatalogSource {
    Embedded,
    Cache(std::path::PathBuf),
}

struct Catalog {
    entries: HashMap<String, Vec<ModelEntry>>,
    #[cfg_attr(not(feature = "multimodal"), allow(dead_code))]
    image_input: HashMap<String, HashMap<String, bool>>,
    max_output: HashMap<String, HashMap<String, u32>>,
    long_context: HashMap<String, HashMap<String, crate::pricing::LongContextPrices>>,
    source: CatalogSource,
    generated_at: Option<jiff::Timestamp>,
}
static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    // Tests never read a developer's real cache directory.
    #[cfg(not(test))]
    let overlay = cache_path();
    #[cfg(test)]
    let overlay: Option<std::path::PathBuf> = None;
    load_catalog(
        CATALOG_JSON,
        EMBEDDED_GENERATED_AT,
        overlay.as_deref(),
        jiff::Timestamp::now(),
    )
});

/// Clock-skew policy for a cache stamped ahead of the host clock: up to one
/// day ahead is tolerated (ordinary skew between machines). Anything later was
/// written while the clock was wrong and would otherwise outrank the embedded
/// snapshot, and suppress every refresh, until real time caught up.
const MAX_FUTURE_SKEW_DAYS: i64 = 1;

fn is_future_dated(at: jiff::Timestamp, now: jiff::Timestamp) -> bool {
    at > now + jiff::SignedDuration::from_hours(MAX_FUTURE_SKEW_DAYS * 24)
}

fn parse_instant(value: &str) -> Option<jiff::Timestamp> {
    value.trim().parse::<jiff::Timestamp>().ok()
}

/// The cache-overlay path, when application paths are available.
pub(crate) fn cache_path() -> Option<std::path::PathBuf> {
    crate::paths::process_paths_ref()
        .ok()
        .map(|paths| paths.cache_dir.join(CACHE_FILE))
}

/// Read and validate an overlay file; any failure keeps the embedded data.
fn read_overlay(path: &std::path::Path) -> Option<CatalogOverlay> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!("model catalog cache unreadable; using the embedded snapshot: {error}");
            return None;
        }
    };
    if bytes.len() > MAX_CATALOG_BYTES {
        tracing::warn!("model catalog cache is too large; using the embedded snapshot");
        return None;
    }
    match serde_json::from_slice::<CatalogOverlay>(&bytes) {
        Ok(overlay) if parse_instant(&overlay.generated_at).is_some() => Some(overlay),
        Ok(_) => {
            tracing::warn!(
                "model catalog cache has no valid generated_at; using the embedded snapshot"
            );
            None
        }
        Err(error) => {
            tracing::warn!("model catalog cache is corrupt; using the embedded snapshot: {error}");
            None
        }
    }
}

fn load_catalog(
    embedded_json: &str,
    embedded_generated_at: &str,
    overlay_path: Option<&std::path::Path>,
    now: jiff::Timestamp,
) -> Catalog {
    let mut raw: RawCatalog = match serde_json::from_str(embedded_json) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(
                "embedded data/models.json is malformed; using an empty model catalog: {error}"
            );
            HashMap::new()
        }
    };
    let embedded_at = parse_instant(embedded_generated_at);
    let mut source = CatalogSource::Embedded;
    let mut generated_at = embedded_at;
    if let Some(path) = overlay_path
        && let Some(overlay) = read_overlay(path)
    {
        let overlay_at = parse_instant(&overlay.generated_at);
        if overlay_at.is_some_and(|at| is_future_dated(at, now)) {
            tracing::warn!(
                "model catalog cache is dated in the future; using the embedded snapshot"
            );
        } else if overlay_at > embedded_at && !overlay.providers.is_empty() {
            for (provider, models) in overlay.providers {
                raw.insert(provider, models);
            }
            source = CatalogSource::Cache(path.to_path_buf());
            generated_at = overlay_at;
        }
    }
    let (entries, image_input, max_output, long_context) = build_parts(raw);
    Catalog {
        entries,
        image_input,
        max_output,
        long_context,
        source,
        generated_at,
    }
}

/// Provider names whose models are native Claude ids under another spelling.
fn claude_route(provider: &str) -> bool {
    matches!(provider, "bedrock" | "aws-bedrock" | "vertex" | "vertex-ai")
}

/// Native Claude id behind a Bedrock / Vertex / gateway spelling:
/// `us.anthropic.claude-sonnet-5-v1:0` -> `claude-sonnet-5`,
/// `claude-sonnet-4-5@20250929` -> `claude-sonnet-4-5-20250929`.
pub(crate) fn native_claude_id(model: &str) -> String {
    let mut name = model.trim();
    if let Some(index) = name.find("anthropic.") {
        name = &name[index + "anthropic.".len()..];
    }
    let mut name = name.replace('@', "-");
    if let Some(index) = name.rfind("-v") {
        let suffix = &name[index + 2..];
        let (major, minor) = suffix.split_once(':').unwrap_or((suffix, "0"));
        if !major.is_empty()
            && major.bytes().all(|b| b.is_ascii_digit())
            && minor.bytes().all(|b| b.is_ascii_digit())
        {
            name.truncate(index);
        }
    }
    name
}

/// Whether `model` names a Claude model under any accepted spelling: bare,
/// Bedrock/Vertex (`anthropic.claude-...`, `us.anthropic.claude-...`,
/// `claude-...@date`) or a gateway path (`bedrock/anthropic.claude-...`,
/// `anthropic/claude-3.5-sonnet`). Only the last `/` segment is considered, so a
/// bare substring such as `declaude-mini` never matches.
pub(crate) fn is_claude_family(model: &str) -> bool {
    let slug = model.trim().strip_prefix('~').unwrap_or(model.trim());
    let segment = slug.rsplit('/').next().unwrap_or(slug);
    native_claude_id(segment).starts_with("claude-")
}

/// `claude-<family>-<major>[-<minor>][-YYYYMMDD]` split into
/// (family, major, minor, dated).
fn parse_claude_version(id: &str) -> Option<(&str, u32, u32, bool)> {
    let rest = id.strip_prefix("claude-")?;
    let (family, version) = rest.split_once('-')?;
    if family.is_empty() || !family.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    let mut parts = version.split('-');
    let major: u32 = parts.next()?.parse().ok()?;
    let (mut minor, mut dated) = (0_u32, false);
    for (index, part) in parts.enumerate() {
        let digits = part.bytes().all(|b| b.is_ascii_digit());
        match (index, part.len()) {
            (0, 1..=2) if digits => minor = part.parse().ok()?,
            (_, 8) if digits => dated = true,
            _ => return None,
        }
    }
    Some((family, major, minor, dated))
}

/// For an official-looking Claude id the catalog does not list yet
/// (`claude-fable-5-2`), the nearest same-family entry: the newest listed
/// version not above it. Its context window, output
/// ceiling and prices are inherited so the new model is not unpriced or left
/// on the 128k fallback (mini-agent-nvnkx.36). Exact entries never come here.
fn nearest_claude_entry(model: &str) -> Option<&'static str> {
    let (family, major, minor, _) = parse_claude_version(model)?;
    let anthropic = CATALOG.entries.get("anthropic")?;
    let mut below: Option<(&'static str, (u32, u32, bool))> = None;
    for entry in anthropic {
        let Some((fam, ma, mi, dated)) = parse_claude_version(&entry.id) else {
            continue;
        };
        if fam != family {
            continue;
        }
        // Undated entries rank above dated ones at the same version.
        let rank = (ma, mi, !dated);
        if (ma, mi) <= (major, minor) && below.is_none_or(|(_, best)| rank > best) {
            below = Some((entry.id.as_str(), rank));
        }
    }
    // Only a model newer than a listed sibling inherits; an older unlisted
    // id is never guessed from a newer sibling's (different) prices.
    below.map(|(id, _)| id)
}

/// The listed entry whose limits and prices an unlisted official-looking
/// Claude id inherits, when it does (for the startup notice).
pub(crate) fn inherited_catalog_entry(provider: &str, model: &str) -> Option<&'static str> {
    let native = if provider == "anthropic" || claude_route(provider) {
        native_claude_id(model)
    } else {
        return None;
    };
    if CATALOG
        .entries
        .get("anthropic")?
        .iter()
        .any(|entry| entry.id == native)
    {
        return None;
    }
    nearest_claude_entry(&native)
}

/// One-line notice that `provider/model` runs on a sibling's limits/prices.
pub(crate) fn inheritance_notice(provider: &str, model: &str) -> Option<String> {
    let from = inherited_catalog_entry(provider, model)?;
    Some(format!(
        "notice: {model} is not in the model catalog yet; using the context window, output limit and prices of {from} (refresh with /models catalog-refresh)"
    ))
}

/// The catalog key for `provider/model`: the exact entry, or for Bedrock and
/// Vertex the native Anthropic id (with or without its date suffix).
fn catalog_key(provider: &str, model: &str) -> Option<(&'static str, String)> {
    // Gemini on Vertex and non-Claude ids on Bedrock / Vertex resolve to
    // their own catalog entries (mini-agent-nvnkx.44): a Vertex Gemini id is a
    // Gemini entry, and any other exact id is looked up across providers.
    let provider = if matches!(provider, "vertex-gemini" | "vertex-ai-gemini") {
        "gemini"
    } else {
        provider
    };
    if claude_route(provider) && !is_claude_family(model) {
        let bare = model.rsplit('/').next().unwrap_or(model);
        let wanted = if provider.starts_with("vertex") {
            ["gemini", "openrouter", "openai"]
        } else {
            ["openrouter", "openai", "gemini"]
        };
        return wanted.iter().find_map(|key| {
            let (key, entries) = CATALOG.entries.get_key_value(*key)?;
            entries
                .iter()
                .any(|entry| entry.id == bare)
                .then(|| (key.as_str(), bare.to_string()))
        });
    }
    let (provider_key, _) = CATALOG.entries.get_key_value(provider).unzip();
    if let Some(provider_key) = provider_key
        && CATALOG.entries[provider_key]
            .iter()
            .any(|entry| entry.id == model)
    {
        return Some((provider_key.as_str(), model.to_string()));
    }
    if provider != "anthropic" && !claude_route(provider) {
        return None;
    }
    let native = native_claude_id(model);
    let anthropic = CATALOG.entries.get("anthropic")?;
    let undated = native
        .rsplit_once('-')
        .filter(|(_, date)| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()))
        .map(|(base, _)| base.to_string());
    [Some(native.clone()), undated]
        .into_iter()
        .flatten()
        .find(|id| anthropic.iter().any(|entry| &entry.id == id))
        .or_else(|| nearest_claude_entry(&native).map(str::to_string))
        .map(|id| ("anthropic", id))
}

/// The catalog entry for `provider/model`, including Bedrock/Vertex Claude
/// ids mapped to their native Anthropic entry.
pub(crate) fn catalog_entry(provider: &str, model: &str) -> Option<&'static ModelEntry> {
    let (provider, model) = catalog_key(provider, model)?;
    CATALOG
        .entries
        .get(provider)?
        .iter()
        .find(|entry| entry.id == model)
}

/// The long-context (prompt-size) premium tier for `provider/model`, or
/// `None` when the catalog lists none.
pub(crate) fn model_long_context(
    provider: &str,
    model: &str,
) -> Option<crate::pricing::LongContextPrices> {
    let (provider, model) = catalog_key(provider, model)?;
    CATALOG.long_context.get(provider)?.get(&model).copied()
}

/// The provider's maximum output tokens per response for `provider/model`,
/// or `None` when the catalog does not list it.
pub(crate) fn model_max_output(provider: &str, model: &str) -> Option<u64> {
    let (provider, model) = catalog_key(provider, model)?;
    CATALOG
        .max_output
        .get(provider)?
        .get(&model)
        .map(|&tokens| u64::from(tokens))
}

/// The largest catalog output ceiling listed for `model` under any provider,
/// for callers that know a model id but not the provider route (reserve sizing).
pub(crate) fn model_max_output_any_provider(model: &str) -> Option<u64> {
    CATALOG
        .max_output
        .values()
        .filter_map(|models| models.get(model))
        .max()
        .map(|&tokens| u64::from(tokens))
}

/// Provider names present in the catalog (tests iterate every listed model;
/// the ACP model picker lists the baked models of usable providers).
#[cfg(any(test, feature = "acp"))]
pub(crate) fn catalog_provider_names() -> Vec<&'static str> {
    CATALOG.entries.keys().map(String::as_str).collect()
}

/// Explicit immutable input modality for a catalog provider/model ID.
/// Unknown gateways remain unknown; this lookup does not guess from model names.
#[cfg(feature = "multimodal")]
pub(crate) fn model_image_input(provider: &str, model: &str) -> Option<bool> {
    let (provider, model) = catalog_key(provider, model)?;
    CATALOG.image_input.get(provider)?.get(&model).copied()
}

/// `/models` line naming the catalog in use and its age.
pub(crate) fn catalog_age_line(now: jiff::Timestamp) -> String {
    describe_age(&CATALOG.source, CATALOG.generated_at, now)
}

fn describe_age(
    source: &CatalogSource,
    generated_at: Option<jiff::Timestamp>,
    now: jiff::Timestamp,
) -> String {
    let origin = match source {
        CatalogSource::Embedded => "embedded snapshot".to_string(),
        CatalogSource::Cache(path) => format!("models.dev cache ({})", path.display()),
    };
    match generated_at {
        Some(at) => {
            let days = (now.duration_since(at).as_secs() / 86_400).max(0);
            format!(
                "model catalog: {origin} from {} ({days} day{} old; /models catalog-refresh updates it)",
                at.strftime("%Y-%m-%d"),
                if days == 1 { "" } else { "s" }
            )
        }
        None => format!("model catalog: {origin} (age unknown)"),
    }
}

#[cfg(test)]
fn parse_catalog(json: &str) -> HashMap<String, Vec<ModelEntry>> {
    parse_catalog_parts(json).0
}

type CatalogParts = (
    HashMap<String, Vec<ModelEntry>>,
    HashMap<String, HashMap<String, bool>>,
    HashMap<String, HashMap<String, u32>>,
    HashMap<String, HashMap<String, crate::pricing::LongContextPrices>>,
);

/// One deserialisation yields both the entry lists and the explicit image flags.
#[cfg(test)]
fn parse_catalog_parts(json: &str) -> CatalogParts {
    build_parts(serde_json::from_str(json).unwrap_or_default())
}

fn build_parts(raw: RawCatalog) -> CatalogParts {
    let mut entries = HashMap::with_capacity(raw.len());
    let mut image_input = HashMap::with_capacity(raw.len());
    let mut max_output = HashMap::with_capacity(raw.len());
    let mut long_context = HashMap::with_capacity(raw.len());
    for (provider, models) in raw {
        let mut flags = HashMap::new();
        let mut outputs = HashMap::new();
        let mut tiers = HashMap::new();
        let list = models
            .into_iter()
            .map(|m| {
                if let Some(image) = m.image_input {
                    flags.insert(m.id.clone(), image);
                }
                if let Some(tokens) = m.max_output.filter(|&tokens| tokens > 0) {
                    outputs.insert(m.id.clone(), tokens);
                }
                if let Some(tier) = m
                    .long_context
                    .filter(crate::pricing::LongContextPrices::is_valid)
                {
                    tiers.insert(m.id.clone(), tier);
                }
                ModelEntry {
                    id: m.id,
                    display: m.name,
                    context_length: m.context,
                    kind: None,
                    input_price: m.input_price,
                    output_price: m.output_price,
                    cache_read_price: m.cache_read_price,
                    cache_write_price: m.cache_write_price,
                }
            })
            .collect();
        entries.insert(provider.clone(), list);
        image_input.insert(provider.clone(), flags);
        max_output.insert(provider.clone(), outputs);
        long_context.insert(provider, tiers);
    }
    (entries, image_input, max_output, long_context)
}

/// Baked model entries for a provider, or `None` when the provider is not in the
/// catalog (custom gateways, ollama — those resolve live).
pub fn catalog_entries(provider: &str) -> Option<&'static [ModelEntry]> {
    CATALOG.entries.get(provider).map(|v| v.as_slice())
}

// ---- models.dev refresh (mirrors scripts/gen-models-catalog.sh) ----

const DENY: &[&str] = &[
    "embedding",
    "embed-",
    "text-embedding",
    "gemini-embedding",
    "whisper",
    "transcribe",
    "tts",
    "-audio",
    "realtime",
    "speech",
    "dall-e",
    "gpt-image",
    "image-generation",
    "imagen",
    "sora",
    "veo",
    "moderation",
    "rerank",
    "aqa",
    "davinci-002",
    "babbage-002",
    "stable-diffusion",
    "flux",
];
const OPENROUTER_VENDORS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "deepseek",
    "x-ai",
    "meta-llama",
    "mistralai",
    "qwen",
    "moonshotai",
    "z-ai",
];

fn chat_model(id: &str, model: &serde_json::Value, cutoff: &str) -> bool {
    let outputs_text = model
        .pointer("/modalities/output")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|output| output.iter().any(|m| m == "text"));
    let lower = id.to_ascii_lowercase();
    let date = model
        .get("last_updated")
        .or_else(|| model.get("release_date"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("0000");
    outputs_text && !DENY.iter().any(|deny| lower.contains(deny)) && date >= cutoff
}

fn raw_entry(model: &serde_json::Value, priced: bool) -> Option<RawModel> {
    let number = |pointer: &str| model.pointer(pointer).and_then(serde_json::Value::as_f64);
    let tokens = |pointer: &str| {
        model
            .pointer(pointer)
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
    };
    let mut entry = RawModel {
        image_input: model
            .pointer("/modalities/input")
            .and_then(serde_json::Value::as_array)
            .map(|input| input.iter().any(|m| m == "image")),
        id: model.get("id")?.as_str()?.to_string(),
        name: model.get("name")?.as_str()?.to_string(),
        context: tokens("/limit/context"),
        max_output: tokens("/limit/output"),
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        long_context: None,
    };
    if priced {
        entry.input_price = number("/cost/input");
        entry.output_price = number("/cost/output");
        entry.cache_read_price = number("/cost/cache_read");
        entry.cache_write_price = number("/cost/cache_write");
        entry.long_context = model
            .pointer("/cost/tiers")
            .and_then(serde_json::Value::as_array)
            .and_then(|tiers| {
                tiers.iter().find_map(|tier| {
                    (tier.pointer("/tier/type")? == "context").then_some(())?;
                    Some(crate::pricing::LongContextPrices {
                        threshold: tier.pointer("/tier/size")?.as_u64()?,
                        input_price: tier.get("input")?.as_f64()?,
                        output_price: tier.get("output")?.as_f64()?,
                        cache_read_price: tier
                            .get("cache_read")
                            .and_then(serde_json::Value::as_f64),
                    })
                })
            });
    }
    Some(entry)
}

/// The mini-agent catalog projection of a models.dev `api.json` document,
/// with the same filters as `scripts/gen-models-catalog.sh`.
fn transform_models_dev(api: &serde_json::Value) -> RawCatalog {
    let models_of = |source: &str| {
        api.pointer(&format!("/{source}/models"))
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flat_map(|models| models.iter())
    };
    let mut catalog = RawCatalog::new();
    for (provider, source, cutoff) in [
        ("anthropic", "anthropic", "2025-04-01"),
        ("openai", "openai", "0000-00-00"),
        ("gemini", "google", "0000-00-00"),
    ] {
        let mut list: Vec<RawModel> = models_of(source)
            .filter(|(id, model)| chat_model(id, model, cutoff))
            .filter(|(_, model)| {
                ["/cost/input", "/cost/output"].iter().all(|pointer| {
                    model
                        .pointer(pointer)
                        .and_then(serde_json::Value::as_f64)
                        .is_some_and(|price| price > 0.0)
                })
            })
            .filter_map(|(_, model)| raw_entry(model, true))
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        if !list.is_empty() {
            catalog.insert(provider.to_string(), list);
        }
    }
    let mut openrouter: Vec<RawModel> = models_of("openrouter")
        .filter(|(id, model)| chat_model(id, model, "0000-00-00"))
        .filter(|(id, _)| {
            id.split('/')
                .next()
                .is_some_and(|vendor| OPENROUTER_VENDORS.contains(&vendor))
        })
        .filter_map(|(_, model)| raw_entry(model, false))
        .collect();
    openrouter.sort_by(|a, b| a.id.cmp(&b.id));
    if !openrouter.is_empty() {
        catalog.insert("openrouter".to_string(), openrouter);
    }
    catalog
}

/// Fetch models.dev and write the cache overlay used from the next start.
/// Returns the cache path and the number of models written.
pub(crate) async fn refresh_cache() -> anyhow::Result<(std::path::PathBuf, usize)> {
    let path = cache_path().ok_or_else(|| anyhow::anyhow!("no cache directory is available"))?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(90))
        .build()?;
    let mut response = client
        .get(MODELS_DEV_URL)
        .send()
        .await?
        .error_for_status()?;
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_CATALOG_BYTES as u64),
        "models.dev document exceeds the size limit"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            chunk.len() <= MAX_CATALOG_BYTES.saturating_sub(body.len()),
            "models.dev document exceeds the size limit"
        );
        body.extend_from_slice(&chunk);
    }
    let api: serde_json::Value = serde_json::from_slice(&body)?;
    let providers = transform_models_dev(&api);
    let count = providers.values().map(Vec::len).sum();
    anyhow::ensure!(count > 0, "models.dev returned no usable models");
    let overlay = CatalogOverlay {
        generated_at: jiff::Timestamp::now()
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
        source: Some(MODELS_DEV_URL.to_string()),
        providers,
    };
    let bytes = serde_json::to_vec_pretty(&overlay)?;
    let write_path = path.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if let Some(parent) = write_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::fs::atomic_write_sync(&write_path, &bytes)
    })
    .await??;
    Ok((path, count))
}

/// Whether the cache at `path` needs a refresh at `now`: it is missing,
/// unreadable, future-dated beyond the skew policy, or at least `days` old.
fn refresh_due(path: &std::path::Path, days: u64, now: jiff::Timestamp) -> bool {
    let ttl = jiff::SignedDuration::from_hours(
        i64::try_from(days)
            .unwrap_or(i64::MAX / 86_400_000)
            .min(36_500)
            * 24,
    );
    let fresh = read_overlay(path)
        .and_then(|overlay| parse_instant(&overlay.generated_at))
        .is_some_and(|at| !is_future_dated(at, now) && now.duration_since(at) < ttl);
    !fresh
}

/// Default `models_catalog_refresh_days` when the key is unset
/// (mini-agent-nvnkx.36). `models_catalog_refresh_days = 0` opts out.
pub(crate) const DEFAULT_REFRESH_DAYS: u64 = 7;

/// The `models_catalog_refresh_days` startup refresh (default every 7 days,
/// `0` disables): when the cache is missing or older than that, refresh it in
/// the background. The first default fetch is announced once, when the cache
/// does not exist yet. Network failures only log; the embedded snapshot stays
/// in use.
pub(crate) fn spawn_refresh_if_stale(days: Option<u64>) {
    let announce = days.is_none();
    let days = days.unwrap_or(DEFAULT_REFRESH_DAYS);
    if days == 0 {
        return;
    }
    let Some(path) = cache_path() else {
        return;
    };
    if !refresh_due(&path, days, jiff::Timestamp::now()) {
        return;
    }
    if announce && !path.exists() {
        eprintln!(
            "notice: fetching the public model catalog (models.dev) in the background to keep model limits and prices current; set models_catalog_refresh_days = 0 to disable"
        );
    }
    tokio::spawn(async {
        match refresh_cache().await {
            Ok((path, count)) => {
                tracing::info!(
                    "model catalog cache refreshed: {count} models at {}",
                    path.display()
                )
            }
            Err(error) => tracing::warn!("model catalog refresh failed: {error}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::parse_catalog;

    fn overlay_json(generated_at: &str) -> String {
        serde_json::json!({
            "generated_at": generated_at,
            "providers": {
                "anthropic": [{"id": "claude-new-9", "name": "New", "context": 500000, "max_output": 128000, "image_input": true, "input_price": 1.0, "output_price": 2.0}]
            }
        })
        .to_string()
    }

    /// The single explicit clock value every catalog decision in these tests uses.
    const NOW: jiff::Timestamp = jiff::Timestamp::constant(1_797_292_800, 0); // 2026-12-15

    const EMBEDDED: &str = r#"{"anthropic":[{"id":"claude-old","name":"Old","context":1000}],"openai":[{"id":"gpt-x","name":"X","context":2000}]}"#;

    #[test]
    fn newer_cache_overlay_replaces_listed_providers() {
        let root = crate::tests::OwnedTestDirectory::new("catalog-overlay");
        let path = root.path().join("models-catalog.json");
        std::fs::write(&path, overlay_json("2026-12-01T00:00:00Z")).unwrap();
        let catalog = super::load_catalog(EMBEDDED, "2026-11-01T00:00:00Z", Some(&path), NOW);
        assert_eq!(catalog.source, super::CatalogSource::Cache(path.clone()));
        let anthropic = &catalog.entries["anthropic"];
        assert_eq!(
            anthropic.len(),
            1,
            "a listed provider is replaced, retiring old ids"
        );
        assert_eq!(anthropic[0].id, "claude-new-9");
        assert_eq!(catalog.max_output["anthropic"]["claude-new-9"], 128_000);
        assert_eq!(
            catalog.entries["openai"][0].id, "gpt-x",
            "unlisted providers stay"
        );
        let line = super::describe_age(
            &catalog.source,
            catalog.generated_at,
            super::parse_instant("2026-12-11T00:00:00Z").unwrap(),
        );
        assert!(
            line.contains("models.dev cache") && line.contains("10 days old"),
            "{line}"
        );
    }

    #[test]
    fn parse_instant_accepts_old_cache_stamps_and_age_uses_utc_date() {
        let a = super::parse_instant("2026-12-01T00:00:00Z").unwrap();
        assert_eq!(super::parse_instant(" 2026-12-01T00:00:00+00:00 "), Some(a));
        assert_eq!(super::parse_instant("2026-12-01T01:00:00+01:00"), Some(a));
        assert_eq!(
            super::parse_instant("2026-12-01T00:00:00.5Z")
                .unwrap()
                .as_millisecond()
                % 1000,
            500
        );
        assert!(super::parse_instant("2026-12-01").is_none());
        assert_eq!(a.strftime("%Y-%m-%d").to_string(), "2026-12-01");
    }

    #[test]
    fn older_or_corrupt_cache_keeps_the_embedded_snapshot() {
        let root = crate::tests::OwnedTestDirectory::new("catalog-overlay-corrupt");
        let path = root.path().join("models-catalog.json");
        for content in [
            overlay_json("2026-10-01T00:00:00Z"),
            overlay_json("not a date"),
            "{truncated".to_string(),
            "[]".to_string(),
        ] {
            std::fs::write(&path, content).unwrap();
            let catalog = super::load_catalog(EMBEDDED, "2026-11-01T00:00:00Z", Some(&path), NOW);
            assert_eq!(catalog.source, super::CatalogSource::Embedded);
            assert_eq!(catalog.entries["anthropic"][0].id, "claude-old");
        }
        let missing = root.path().join("absent.json");
        let catalog = super::load_catalog(EMBEDDED, "2026-11-01T00:00:00Z", Some(&missing), NOW);
        assert_eq!(catalog.source, super::CatalogSource::Embedded);
        let line = super::describe_age(
            &catalog.source,
            catalog.generated_at,
            super::parse_instant("2026-11-02T00:00:00Z").unwrap(),
        );
        assert!(
            line.contains("embedded snapshot from 2026-11-01 (1 day old"),
            "{line}"
        );
    }

    #[test]
    fn future_dated_overlay_is_ignored_and_triggers_refresh_after_clock_rollback() {
        let root = crate::tests::OwnedTestDirectory::new("catalog-overlay-future");
        let path = root.path().join("models-catalog.json");
        let embedded_at = "2026-11-01T00:00:00Z";
        // (generated_at, selected over embedded, refresh due with 7-day TTL)
        for (stamp, selected, due) in [
            ("2026-12-14T00:00:00Z", true, false), // current: one day old
            ("2026-12-15T12:00:00Z", true, false), // within the skew allowance
            ("2026-12-10T00:00:00Z", true, false), // fresh
            ("2026-12-01T00:00:00Z", true, true),  // expired
            ("2027-06-01T00:00:00Z", false, true), // clock was ahead: future
            ("9999-01-01T00:00:00Z", false, true),
            ("2026-10-01T00:00:00Z", false, true), // embedded newer
            ("not a date", false, true),           // invalid
        ] {
            std::fs::write(&path, overlay_json(stamp)).unwrap();
            let catalog = super::load_catalog(EMBEDDED, embedded_at, Some(&path), NOW);
            assert_eq!(
                catalog.source == super::CatalogSource::Cache(path.clone()),
                selected,
                "{stamp}"
            );
            assert_eq!(super::refresh_due(&path, 7, NOW), due, "{stamp}");
        }
        std::fs::remove_file(&path).unwrap();
        assert!(super::refresh_due(&path, 7, NOW), "missing cache refreshes");
    }

    #[test]
    fn bedrock_and_vertex_claude_ids_use_native_metadata() {
        let entry = super::catalog_entries("anthropic")
            .unwrap()
            .iter()
            .find(|entry| entry.id == "claude-sonnet-4-5")
            .expect("embedded claude-sonnet-4-5");
        for (provider, model) in [
            ("bedrock", "us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            ("bedrock", "anthropic.claude-sonnet-4-5"),
            ("vertex", "claude-sonnet-4-5@20250929"),
        ] {
            let mapped = super::catalog_entry(provider, model)
                .unwrap_or_else(|| panic!("{provider}/{model}"));
            assert_eq!(mapped.context_length, entry.context_length);
            assert_eq!(mapped.input_price, entry.input_price);
            assert_eq!(
                super::model_max_output(provider, model),
                super::model_max_output("anthropic", "claude-sonnet-4-5")
            );
        }
        assert!(super::catalog_entry("bedrock", "amazon.nova-pro-v1:0").is_none());
        assert!(super::catalog_entry("ollama", "claude-sonnet-4-5").is_none());
        assert_eq!(
            crate::config::Config::catalog_context_window("vertex", "claude-sonnet-4-5@20250929"),
            entry.context_length.map(u64::from)
        );
    }

    #[test]
    fn models_dev_transform_mirrors_the_generator_filters() {
        let api = serde_json::json!({
            "anthropic": {"models": {
                "claude-keep": {"id": "claude-keep", "name": "Keep", "release_date": "2026-01-01",
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "limit": {"context": 200000, "output": 64000},
                    "cost": {"input": 3, "output": 15, "cache_read": 0.3,
                             "tiers": [{"input": 6, "output": 22.5, "tier": {"type": "context", "size": 200000}}]}},
                "claude-3-old": {"id": "claude-3-old", "name": "Old", "release_date": "2024-01-01",
                    "modalities": {"input": ["text"], "output": ["text"]}, "cost": {"input": 1, "output": 2}},
                "claude-free": {"id": "claude-free", "name": "Free", "release_date": "2026-01-01",
                    "modalities": {"input": ["text"], "output": ["text"]}, "cost": {"input": 0, "output": 0}}
            }},
            "openai": {"models": {
                "text-embedding-3": {"id": "text-embedding-3", "name": "E", "modalities": {"output": ["text"]}, "cost": {"input": 1, "output": 1}}
            }},
            "openrouter": {"models": {
                "anthropic/claude-keep": {"id": "anthropic/claude-keep", "name": "K", "last_updated": "2026-02-01", "modalities": {"output": ["text"]}, "limit": {"output": 1000}},
                "someone/else": {"id": "someone/else", "name": "S", "last_updated": "2026-02-01", "modalities": {"output": ["text"]}}
            }}
        });
        let catalog = super::transform_models_dev(&api);
        let anthropic = &catalog["anthropic"];
        assert_eq!(anthropic.len(), 1);
        let keep = &anthropic[0];
        assert_eq!(keep.id, "claude-keep");
        assert_eq!(
            (keep.context, keep.max_output, keep.image_input),
            (Some(200_000), Some(64_000), Some(true))
        );
        assert_eq!(keep.long_context.unwrap().threshold, 200_000);
        assert!(!catalog.contains_key("openai"), "embeddings are denied");
        let openrouter = &catalog["openrouter"];
        assert_eq!(openrouter.len(), 1);
        assert_eq!(
            openrouter[0].input_price, None,
            "OpenRouter prices stay live"
        );
    }

    #[test]
    fn catalog_max_output_is_parsed_and_embedded() {
        let (_, _, outputs, _) = super::parse_catalog_parts(
            r#"{"anthropic":[{"id":"a","name":"a","context":1000,"max_output":64000},{"id":"b","name":"b","context":1000},{"id":"c","name":"c","max_output":0}]}"#,
        );
        assert_eq!(outputs["anthropic"].get("a"), Some(&64_000));
        assert_eq!(outputs["anthropic"].get("b"), None);
        assert_eq!(outputs["anthropic"].get("c"), None);
        // Every embedded direct-API entry carries its output ceiling.
        for provider in ["anthropic", "openai"] {
            for entry in super::catalog_entries(provider).unwrap() {
                assert!(
                    super::model_max_output(provider, &entry.id).is_some(),
                    "{provider}/{} lacks max_output",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn malformed_catalog_falls_back_to_empty() {
        let catalog = super::load_catalog("{not valid json", "", None, NOW);

        assert!(catalog.entries.is_empty());
        assert!(parse_catalog("{not valid json").is_empty());
    }

    #[test]
    fn catalog_cache_pricing_accepts_legacy_missing_and_explicit_zero_tiers() {
        let parsed = parse_catalog(
            r#"{"anthropic":[{"id":"legacy","name":"legacy","context":1000,"input_price":4,"output_price":20},{"id":"priced","name":"priced","context":1000,"input_price":4,"output_price":20,"cache_read_price":0,"cache_write_price":5}]}"#,
        );
        assert_eq!(parsed["anthropic"][0].cache_read_price, None);
        assert_eq!(parsed["anthropic"][0].cache_write_price, None);
        assert_eq!(parsed["anthropic"][1].cache_read_price, Some(0.0));
        assert_eq!(parsed["anthropic"][1].cache_write_price, Some(5.0));
    }

    #[test]
    fn built_in_provider_defaults_exist_in_the_catalog() {
        let cfg = crate::config::Config::default();
        for provider in ["anthropic", "openai", "gemini"] {
            let (model, _) = crate::provider::default_model_for_provider(provider, &cfg)
                .unwrap_or_else(|| panic!("no default for {provider}"));
            assert!(
                super::catalog_entry(provider, &model).is_some(),
                "{provider} default {model} is missing from data/models.json"
            );
        }
        // No newer flash model than the Gemini default is listed.
        let flash = |id: &str| -> Option<(u32, u32)> {
            let rest = id.strip_prefix("gemini-")?.strip_suffix("-flash")?;
            let (major, minor) = rest.split_once('.').unwrap_or((rest, "0"));
            Some((major.parse().ok()?, minor.parse().ok()?))
        };
        let (gemini, _) = crate::provider::default_model_for_provider("gemini", &cfg).unwrap();
        let newest = super::catalog_entries("gemini")
            .unwrap()
            .iter()
            .filter_map(|entry| flash(&entry.id))
            .max()
            .unwrap();
        assert_eq!(flash(&gemini), Some(newest));
    }

    #[test]
    fn unlisted_newer_claude_ids_inherit_the_family_entry() {
        let listed = super::catalog_entry("anthropic", "claude-fable-5-1").unwrap();
        let inherited = super::catalog_entry("anthropic", "claude-fable-5-2").unwrap();
        assert_eq!(inherited.id, listed.id);
        assert_eq!(inherited.context_length, listed.context_length);
        assert_eq!(inherited.input_price, listed.input_price);
        let notice = super::inheritance_notice("anthropic", "claude-fable-5-2").unwrap();
        assert!(notice.contains("claude-fable-5-1"), "{notice}");
        assert!(super::inheritance_notice("anthropic", "claude-fable-5-1").is_none());
        // Aliases and older unlisted ids are never guessed.
        assert!(super::catalog_entry("anthropic", "claude-custom-alias").is_none());
        assert!(super::catalog_entry("anthropic", "claude-3-7-sonnet").is_none());
        assert!(super::catalog_entry("anthropic", "claude-fable-4-1").is_none());
    }
}
