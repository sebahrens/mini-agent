use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use compact_str::CompactString;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rig::agent::Agent;
use rig::client::{CompletionClient, ModelListingClient};
use rig::completion::{CompletionModel, Message};
use rig::providers::{anthropic, gemini, ollama, openai, openrouter};
use rig::streaming::StreamingChat;
use tokio::sync::mpsc;

use crate::agent::builder;
use crate::agent::prompt;
use crate::agent::runner::{self, AgentRunner};
use crate::auth::{AuthResolver, ProviderKind};
use crate::cli::Cli;
use crate::config::{ApiStyle, Config, CustomProviderConfig};
use crate::context::ContextFiles;
#[cfg(any(feature = "hooks", feature = "subagents"))]
use crate::event::AgentEvent;
#[cfg(feature = "hooks")]
use crate::extras::hooks::LoopInfo;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::retry::{self, RetryConfig};
use crate::sandbox::Sandbox;
use crate::session::SessionMessage;

const MAX_COMPACTION_REQUESTS: usize = 16;
const MAX_ROLLING_SUMMARY_BYTES: usize = 16 * 1024;
const COMPACTION_TIMEOUT: Duration = Duration::from_secs(300);
const PROVIDER_ENVELOPE_TOKEN_RESERVE: u64 = 512;
const MIN_SUMMARY_OUTPUT_TOKENS: u64 = 256;
const MAX_SUMMARY_OUTPUT_TOKENS: u64 = 4_096;
const NO_PREVIOUS_SUMMARY: &str = "(none)";
const OLDER_HISTORY_OMITTED: &str = "[older history omitted to bound compaction work]\n";

pub(crate) fn compaction_request_limits(
    input_token_budget: u64,
    response_token_budget: u64,
    preamble_bytes: usize,
) -> (usize, u64) {
    let max_output_tokens =
        response_token_budget.clamp(MIN_SUMMARY_OUTPUT_TOKENS, MAX_SUMMARY_OUTPUT_TOKENS);
    let borrowed_output_headroom = max_output_tokens.saturating_sub(response_token_budget);
    let bounded_input_tokens = input_token_budget
        .saturating_sub(borrowed_output_headroom)
        .saturating_sub(PROVIDER_ENVELOPE_TOKEN_RESERVE);
    let request_budget = bounded_input_tokens.try_into().unwrap_or(usize::MAX);
    (
        request_budget.saturating_sub(preamble_bytes),
        max_output_tokens,
    )
}

pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub api_key_env: Option<CompactString>,
    pub danger_accept_invalid_certs: bool,
}

pub fn resolve_provider_config(
    name: &str,
    custom_providers: &HashMap<String, CustomProviderConfig>,
) -> anyhow::Result<ProviderConfig> {
    if let Some(custom) = custom_providers.get(name) {
        let kind = ProviderKind::from_name(&custom.provider_type).ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown provider type: {}. Run `mini-agent --setup` to configure providers.",
                custom.provider_type
            )
        })?;
        return Ok(ProviderConfig {
            kind,
            base_url: Some(custom.base_url.clone()),
            api_key_env: custom.api_key_env.clone(),
            danger_accept_invalid_certs: custom.danger_accept_invalid_certs.unwrap_or(false),
        });
    }
    let kind = ProviderKind::from_name(name).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown provider: '{}'. Supported: openrouter, openai, anthropic, gemini, ollama. Run `mini-agent --setup` to configure providers.",
            name
        )
    })?;

    Ok(ProviderConfig {
        kind,
        base_url: None,
        api_key_env: None,
        danger_accept_invalid_certs: false,
    })
}

/// Re-exported for compatibility with existing code
pub fn parse_provider(name: &str) -> Option<ProviderKind> {
    ProviderKind::from_name(name)
}

/// Pick a sensible default model when targeting `provider`. Priority:
/// a custom gateway's configured `model`, then a quick model targeting this
/// provider (carrying its pricing), then a built-in fallback. Returns
/// (model, Option<(input_cost, output_cost)>), or None if `provider` is unknown
/// and has no configured default. Used both by `/provider` and at startup so a
/// chosen provider never keeps an id that is invalid on it.
pub(crate) fn default_model_for_provider(
    provider: &str,
    cfg: &Config,
) -> Option<(String, Option<(f64, f64)>)> {
    if let Some(c) = cfg.custom_providers_map().get(provider)
        && let Some(m) = &c.model
    {
        return Some((m.to_string(), None));
    }
    // Deterministic: prefer the alphabetically-first quick model for this provider
    // (HashMap iteration order would otherwise be unstable).
    let qm = crate::config::quick_models_map(cfg);
    let mut names: Vec<&String> = qm.keys().collect();
    names.sort();
    for name in names {
        let q = &qm[name];
        if q.provider.as_str() == provider {
            return Some((
                q.model.to_string(),
                Some((q.input_token_cost, q.output_token_cost)),
            ));
        }
    }
    let m = match provider {
        "anthropic" => "claude-sonnet-4-6",
        "openai" => "gpt-5.1",
        "gemini" | "google" => "gemini-2.5-pro",
        "openrouter" => "openrouter/auto", // OpenRouter's always-valid auto-router
        "ollama" => "llama3.1",
        _ => return None,
    };
    Some((m.to_string(), None))
}

fn resolve_base_url(config: &ProviderConfig) -> Option<String> {
    config.base_url.clone()
}

/// rig 0.37 exposes two distinct OpenAI client types:
/// - `openai::Client`            -> Responses API (`/responses`). Real OpenAI,
///   including GPT-5; rig maps `max_tokens` to `max_output_tokens`, so it does
///   not hit the GPT-5 400.
/// - `openai::CompletionsClient` -> Chat Completions API (`/chat/completions`).
///   Most OpenAI-compatible gateways (vLLM / LiteLLM / self-hosted) implement
///   only this endpoint.
///
/// The two cannot share a single type, so we wrap them in an inner enum and let
/// `ApiStyle` decide which one to build.
#[derive(Clone)]
pub enum OpenAiClient {
    Responses(openai::Client),
    Completions(openai::CompletionsClient),
}

impl OpenAiClient {
    fn completion_model(&self, name: String) -> OpenAiModel {
        match self {
            OpenAiClient::Responses(c) => OpenAiModel::Responses(c.completion_model(name)),
            OpenAiClient::Completions(c) => OpenAiModel::Completions(c.completion_model(name)),
        }
    }
}

pub enum OpenAiModel {
    Responses(openai::responses_api::ResponsesCompletionModel),
    Completions(openai::completion::CompletionModel),
}

#[derive(Clone)]
pub enum OpenAiAgent {
    Responses(Agent<openai::responses_api::ResponsesCompletionModel>),
    Completions(Agent<openai::completion::CompletionModel>),
}

#[derive(Clone)]
pub enum AnyClient {
    OpenRouter(openrouter::Client),
    OpenAI(OpenAiClient),
    Anthropic(anthropic::Client),
    Gemini(gemini::Client),
    Ollama(ollama::Client),
}

/// Extra OpenRouter request body params that pin a Claude model to the
/// Anthropic direct route, or `None` for any non-Claude model.
///
/// `cache_control` breakpoints (used for prompt caching) are only honored on
/// OpenRouter's Anthropic direct route; the Bedrock and Vertex routes silently
/// drop them. So for Claude models we force `provider.order = ["Anthropic"]`
/// (keeping `allow_fallbacks: true` so the request still succeeds if Anthropic
/// is momentarily unavailable). Every other OpenRouter model caches
/// automatically and is left untouched.
///
/// OpenRouter namespaces Claude under `anthropic/`, optionally with a leading
/// `~` marking a floating "-latest" alias (e.g. `~anthropic/claude-sonnet-latest`).
/// The `~` is part of the real slug, so strip it before matching.
pub(crate) fn openrouter_anthropic_routing(model_id: &str) -> Option<serde_json::Value> {
    let slug = model_id.strip_prefix('~').unwrap_or(model_id);
    slug.starts_with("anthropic/").then(|| {
        serde_json::json!({
            "provider": { "order": ["Anthropic"], "allow_fallbacks": true }
        })
    })
}

/// Shallow-merges user-configured `extra_body` into provider-internal routing
/// params (e.g. OpenRouter's `provider.order`). Top-level keys from `extra_body`
/// win on collision. Returns `None` when both are absent so callers can avoid an
/// empty `additional_params` call.
pub(crate) fn merge_extra_body(
    base: Option<serde_json::Value>,
    extra: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    match (base, extra) {
        (Some(serde_json::Value::Object(mut b)), Some(serde_json::Value::Object(e))) => {
            b.extend(e);
            Some(serde_json::Value::Object(b))
        }
        (base, None) => base,
        (None, extra) => extra,
        // Non-object base (shouldn't happen for routing) — user value takes over.
        (Some(_), extra) => extra,
    }
}

impl AnyClient {
    #[allow(dead_code)]
    pub fn provider_name(&self) -> &'static str {
        match self {
            AnyClient::OpenRouter(_) => "openrouter",
            AnyClient::OpenAI(_) => "openai",
            AnyClient::Anthropic(_) => "anthropic",
            AnyClient::Gemini(_) => "gemini",
            AnyClient::Ollama(_) => "ollama",
        }
    }

    pub fn completion_model(&self, name: impl Into<String>) -> AnyModel {
        let name = name.into();
        match self {
            AnyClient::OpenRouter(c) => {
                let extra = openrouter_anthropic_routing(&name);
                AnyModel::OpenRouter(c.completion_model(name).with_prompt_caching(), extra)
            }
            AnyClient::OpenAI(c) => AnyModel::OpenAI(c.completion_model(name)),
            AnyClient::Anthropic(c) => {
                AnyModel::Anthropic(c.completion_model(name).with_prompt_caching())
            }
            AnyClient::Gemini(c) => AnyModel::Gemini(c.completion_model(name)),
            AnyClient::Ollama(c) => AnyModel::Ollama(c.completion_model(name)),
        }
    }

    pub async fn compress_messages(
        &self,
        model_name: &str,
        messages: &[SessionMessage],
        previous_summary: Option<&str>,
        instructions: Option<&str>,
        input_token_budget: u64,
        response_token_budget: u64,
    ) -> anyhow::Result<(String, usize)> {
        let preamble = summarizer_preamble();
        let (prompt_budget, max_output_tokens) =
            compaction_request_limits(input_token_budget, response_token_budget, preamble.len());
        // Without a provider tokenizer, one UTF-8 byte per configured token is
        // the provider-neutral conservative fallback. `input_token_budget`
        // already excludes the configured response reserve. We also reserve a
        // provider-envelope allowance and borrow enough input headroom to keep
        // a useful response cap when a custom reserve is smaller than 256.

        // Determine how many messages fit within the budget, keeping the most recent.
        let (conversation, messages_included) =
            serialize_conversation_bounded(messages, prompt_budget)?;

        tokio::time::timeout(
            COMPACTION_TIMEOUT,
            summarize_conversation_bounded(
                &conversation,
                previous_summary,
                instructions,
                prompt_budget,
                |summary_prompt| {
                    let preamble = preamble.clone();
                    async move {
                        let model = self.completion_model(model_name.to_string());
                        summarize_with_model(model, summary_prompt, preamble, max_output_tokens)
                            .await
                    }
                },
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Compression timed out after 300 seconds"))?
        .map(|summary| (summary, messages_included))
    }
}

fn compaction_prompt(
    conversation: &str,
    previous_summary: Option<&str>,
    instructions: Option<&str>,
) -> String {
    let template = prompt::COMPACTION_PROMPT;
    let (before_summary, after_summary) = template
        .split_once("{previous_summary}")
        .expect("compaction prompt has previous-summary placeholder");
    let (before_instructions, after_instructions) = after_summary
        .split_once("{instructions}")
        .expect("compaction prompt has instructions placeholder");
    let (before_conversation, after_conversation) = after_instructions
        .split_once("{conversation}")
        .expect("compaction prompt has conversation placeholder");
    let previous_summary = previous_summary.unwrap_or(NO_PREVIOUS_SUMMARY);
    let instructions = instructions.unwrap_or("(none)");

    let mut rendered = String::with_capacity(
        template.len() + previous_summary.len() + instructions.len() + conversation.len(),
    );
    rendered.push_str(before_summary);
    rendered.push_str(previous_summary);
    rendered.push_str(before_instructions);
    rendered.push_str(instructions);
    rendered.push_str(before_conversation);
    rendered.push_str(conversation);
    rendered.push_str(after_conversation);
    rendered
}

fn prefix_at_most(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn suffix_at_most(value: &str, max_bytes: usize) -> &str {
    let mut start = value.len().saturating_sub(max_bytes);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

fn bound_summary(value: &str, max_bytes: usize) -> String {
    const MARKER: &str = "\n...[summary truncated]...\n";

    if value.len() <= max_bytes {
        return value.to_string();
    }
    if max_bytes <= MARKER.len() {
        return prefix_at_most(MARKER, max_bytes).to_string();
    }

    let content_budget = max_bytes - MARKER.len();
    let head = prefix_at_most(value, content_budget / 2);
    let tail = suffix_at_most(value, content_budget.saturating_sub(head.len()));
    format!("{head}{MARKER}{tail}")
}

fn compaction_payload_budgets(
    instructions: Option<&str>,
    prompt_budget_bytes: usize,
) -> anyhow::Result<(usize, usize)> {
    let fixed_overhead = compaction_prompt("", Some(""), instructions).len();
    let available = prompt_budget_bytes.saturating_sub(fixed_overhead);
    if available <= NO_PREVIOUS_SUMMARY.len() + 4 {
        anyhow::bail!("Compression prompt metadata exceeds the bounded summarizer input budget");
    }

    let summary_budget = (available / 2)
        .max(NO_PREVIOUS_SUMMARY.len())
        .min(MAX_ROLLING_SUMMARY_BYTES);
    let conversation_budget = available.saturating_sub(summary_budget);
    if conversation_budget < 4 {
        anyhow::bail!("Compression input budget cannot fit one UTF-8 character");
    }
    Ok((summary_budget, conversation_budget))
}

/// Serializes messages to a bounded conversation string, keeping the most recent messages
/// and discarding older ones if needed to fit the budget. Returns both the serialized
/// conversation (with OLDER_HISTORY_OMITTED marker if applicable) and the count of
/// messages that were actually included. This ensures that only messages whose content
/// was sent to the summarizer are later deleted from the session.
fn serialize_conversation_bounded(
    messages: &[SessionMessage],
    prompt_budget_bytes: usize,
) -> anyhow::Result<(String, usize)> {
    // Use the full prompt budget as the conversation budget when the overhead
    // computation fails (e.g. budget is smaller than the compaction prompt
    // wrapper). The "always include at least one message" fallback below
    // guarantees the function never returns an empty result.
    let conversation_budget = compaction_payload_budgets(None, prompt_budget_bytes)
        .map(|(_, c)| c)
        .unwrap_or(prompt_budget_bytes);

    // Calculate how many recent messages fit within the budget, keeping those messages.
    // We work backwards from the end (most recent) to include as much recent context
    // as possible, which is important for summarization.
    let mut accumulated_size = 0usize;
    let mut messages_to_include = 0usize;

    for msg in messages.iter().rev() {
        let role_tag = match msg.role {
            crate::session::MessageRole::User => "User",
            crate::session::MessageRole::Assistant => "Assistant",
            crate::session::MessageRole::System => "System",
            crate::session::MessageRole::ToolCall => "ToolCall",
            crate::session::MessageRole::ToolResult => "ToolResult",
            crate::session::MessageRole::SubagentToolCall => "SubagentToolCall",
        };
        let serialized = format!("[{}]: {}\n\n", role_tag, msg.content);

        if accumulated_size + serialized.len() <= conversation_budget {
            accumulated_size += serialized.len();
            messages_to_include += 1;
        } else if messages_to_include == 0 {
            // Must fit at least one message, even if it exceeds budget slightly
            messages_to_include = 1;
            accumulated_size = serialized.len();
            break;
        } else {
            break;
        }
    }

    // Serialize the included messages in the correct (chronological) order
    let start_idx = messages.len().saturating_sub(messages_to_include);
    let mut conversation = String::new();

    // If we're not including all messages, add the marker
    let all_included = start_idx == 0;
    if !all_included {
        conversation.push_str(OLDER_HISTORY_OMITTED);
    }

    for msg in &messages[start_idx..] {
        let role_tag = match msg.role {
            crate::session::MessageRole::User => "User",
            crate::session::MessageRole::Assistant => "Assistant",
            crate::session::MessageRole::System => "System",
            crate::session::MessageRole::ToolCall => "ToolCall",
            crate::session::MessageRole::ToolResult => "ToolResult",
            crate::session::MessageRole::SubagentToolCall => "SubagentToolCall",
        };
        conversation.push_str(&format!("[{}]: {}\n\n", role_tag, msg.content));
    }

    Ok((conversation, messages_to_include))
}

fn bounded_recent_conversation<'a>(
    conversation: &'a str,
    conversation_budget: usize,
) -> anyhow::Result<Cow<'a, str>> {
    // A UTF-8 boundary can leave at most three unused bytes in each request.
    let usable_per_request = conversation_budget.saturating_sub(3);
    let retained_budget = usable_per_request.saturating_mul(MAX_COMPACTION_REQUESTS);
    if conversation.len() <= retained_budget {
        return Ok(Cow::Borrowed(conversation));
    }
    if retained_budget <= OLDER_HISTORY_OMITTED.len() {
        anyhow::bail!("Compression input budget is too small for bounded history processing");
    }

    let tail = suffix_at_most(conversation, retained_budget - OLDER_HISTORY_OMITTED.len());
    Ok(Cow::Owned(format!("{OLDER_HISTORY_OMITTED}{tail}")))
}

/// Summarizes bounded recent slices of a conversation through a rolling
/// reduction. Every request has a fixed history and summary partition, and the
/// total number of provider calls is capped.
pub(crate) async fn summarize_conversation_bounded<F, Fut>(
    conversation: &str,
    previous_summary: Option<&str>,
    instructions: Option<&str>,
    prompt_budget_bytes: usize,
    mut summarize: F,
) -> anyhow::Result<String>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let (summary_budget, conversation_budget) =
        compaction_payload_budgets(instructions, prompt_budget_bytes)?;
    let conversation = bounded_recent_conversation(conversation, conversation_budget)?;
    let mut offset = 0usize;
    let mut rolling_summary =
        previous_summary.map(|summary| bound_summary(summary, summary_budget));
    let mut made_request = false;
    let mut requests = 0usize;

    loop {
        let remaining = &conversation[offset..];
        if remaining.is_empty() && made_request {
            return rolling_summary
                .filter(|summary| !summary.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Compression returned empty response"));
        }

        let chunk = prefix_at_most(remaining, conversation_budget);
        if !remaining.is_empty() && chunk.is_empty() {
            anyhow::bail!("Compression input budget cannot fit one UTF-8 character");
        }
        let request = compaction_prompt(chunk, rolling_summary.as_deref(), instructions);
        debug_assert!(request.len() <= prompt_budget_bytes);
        if request.len() > prompt_budget_bytes {
            anyhow::bail!("Compression request exceeded its bounded input budget");
        }
        if requests >= MAX_COMPACTION_REQUESTS {
            anyhow::bail!("Compression exceeded its bounded provider request count");
        }

        let next_summary = summarize(request).await?;
        if next_summary.is_empty() {
            anyhow::bail!("Compression returned empty response");
        }
        rolling_summary = Some(bound_summary(&next_summary, summary_budget));
        made_request = true;
        requests += 1;
        offset = offset.saturating_add(chunk.len());
    }
}

#[derive(Clone)]
pub struct ModelEntry {
    pub id: String,
    pub display: String,
    pub context_length: Option<u32>,
    pub kind: Option<String>,
    pub input_price: Option<f64>,
    pub output_price: Option<f64>,
}

impl ModelEntry {
    fn from_rig(m: &rig::model::listing::Model) -> Self {
        Self {
            id: m.id.clone(),
            display: m.display_name().to_string(),
            context_length: m.context_length,
            kind: m.r#type.clone(),
            input_price: None,
            output_price: None,
        }
    }
}

/// Chat/completion model suitable as an agent (not embedding/image/audio/etc.)?
pub fn is_agent_model(m: &ModelEntry) -> bool {
    if let Some(t) = m.kind.as_deref() {
        let t = t.to_lowercase();
        if [
            "embed",
            "image",
            "audio",
            "video",
            "moderation",
            "rerank",
            "tts",
            "speech",
        ]
        .iter()
        .any(|k| t.contains(k))
        {
            return false;
        }
    }
    let id = m.id.to_lowercase();
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
    ];
    !DENY.iter().any(|d| id.contains(d))
}

impl AnyClient {
    /// Built-in providers: rig's ModelListingClient.
    pub async fn list_models(&self) -> anyhow::Result<Vec<ModelEntry>> {
        let list = match self {
            AnyClient::OpenAI(OpenAiClient::Responses(c)) => c.list_models().await?,
            AnyClient::Anthropic(c) => c.list_models().await?,
            AnyClient::OpenRouter(c) => c.list_models().await?,
            AnyClient::Gemini(c) => c.list_models().await?,
            AnyClient::Ollama(c) => c.list_models().await?,
            // If any arm above does NOT impl ModelListingClient it won't compile —
            // move it down here to the manual fallback.
            AnyClient::OpenAI(OpenAiClient::Completions(_)) => {
                anyhow::bail!("rig model listing unavailable for this client")
            }
        };
        Ok(list.iter().map(ModelEntry::from_rig).collect())
    }
}

/// Custom / OpenAI-compatible gateway: best-effort GET {base}/models.
pub async fn list_models_manual(
    provider_name: &str,
    cli_key: Option<&str>,
    custom_providers: &std::collections::HashMap<String, CustomProviderConfig>,
    config_api_keys: Option<&std::collections::HashMap<String, String>>,
) -> anyhow::Result<Vec<ModelEntry>> {
    let config = resolve_provider_config(provider_name, custom_providers)?;
    let base = config
        .base_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no base_url"))?;
    let key = AuthResolver::new(config.kind)
        .with_cli_key(cli_key)
        .with_env_override(config.api_key_env.as_deref())
        .with_config_keys(config_api_keys)
        .with_custom_provider_name(Some(provider_name))
        .resolve()
        .ok();
    let custom = custom_providers.get(provider_name);
    let http = build_http_client(
        provider_name,
        config.danger_accept_invalid_certs,
        custom,
        Some(&base),
    )?;
    let url = format!("{}/models", base.trim_end_matches('/'));
    let mut req = http.get(url);
    if let Some(k) = key.as_deref().filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Vec<Item>,
    }
    #[derive(serde::Deserialize)]
    struct Item {
        id: String,
    }
    let resp: Resp = req.send().await?.error_for_status()?.json().await?;
    Ok(resp
        .data
        .into_iter()
        .map(|i| ModelEntry {
            display: i.id.clone(),
            id: i.id,
            context_length: None,
            kind: None,
            input_price: None,
            output_price: None,
        })
        .collect())
}

#[derive(Clone, Copy, Default)]
pub struct OpenRouterModelInfo {
    pub input_cost: f64,
    pub output_cost: f64,
    pub context_length: Option<u64>,
}

pub async fn fetch_openrouter_pricing(
    api_key: Option<&str>,
    custom_providers: &HashMap<String, CustomProviderConfig>,
    config_api_keys: Option<&HashMap<String, String>>,
) -> anyhow::Result<HashMap<String, OpenRouterModelInfo>> {
    fetch_openrouter_pricing_from_url(
        api_key,
        custom_providers,
        config_api_keys,
        "https://openrouter.ai/api/v1/models",
    )
    .await
}

pub(crate) async fn fetch_openrouter_pricing_from_url(
    api_key: Option<&str>,
    custom_providers: &HashMap<String, CustomProviderConfig>,
    config_api_keys: Option<&HashMap<String, String>>,
    url: &str,
) -> anyhow::Result<HashMap<String, OpenRouterModelInfo>> {
    const MAX_PRICING_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
    let config = resolve_provider_config("openrouter", custom_providers)?;
    let key = AuthResolver::new(config.kind)
        .with_cli_key(api_key)
        .with_env_override(config.api_key_env.as_deref())
        .with_config_keys(config_api_keys)
        .with_custom_provider_name(Some("openrouter"))
        .resolve()
        .ok();
    let custom = custom_providers.get("openrouter");
    let http = build_http_client(
        "openrouter",
        config.danger_accept_invalid_certs,
        custom,
        None,
    )?;
    let mut req = http.get(url);
    if let Some(k) = key.as_deref().filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    #[derive(serde::Deserialize)]
    struct PricingResp {
        prompt: String,
        completion: String,
    }
    #[derive(serde::Deserialize)]
    struct PricingEntry {
        id: String,
        pricing: Option<PricingResp>,
        context_length: Option<u64>,
    }
    #[derive(serde::Deserialize)]
    struct PricingList {
        data: Vec<PricingEntry>,
    }
    let mut response = req.send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|bytes| bytes > MAX_PRICING_RESPONSE_BYTES as u64)
    {
        anyhow::bail!("OpenRouter pricing response exceeded its size limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_PRICING_RESPONSE_BYTES {
            anyhow::bail!("OpenRouter pricing response exceeded its size limit");
        }
        body.extend_from_slice(&chunk);
    }
    let resp: PricingList = serde_json::from_slice(&body)?;
    let mut map = HashMap::new();
    for entry in resp.data {
        let (input, output) = match entry.pricing.as_ref() {
            Some(p) => (
                p.prompt.parse().unwrap_or(0.0),
                p.completion.parse().unwrap_or(0.0),
            ),
            None => (0.0, 0.0),
        };
        if input > 0.0 || output > 0.0 || entry.context_length.is_some() {
            map.insert(
                entry.id,
                OpenRouterModelInfo {
                    input_cost: input * 1_000_000.0,
                    output_cost: output * 1_000_000.0,
                    context_length: entry.context_length,
                },
            );
        }
    }
    Ok(map)
}

async fn summarize_with_model(
    model: AnyModel,
    prompt: String,
    preamble: String,
    max_output_tokens: u64,
) -> anyhow::Result<String> {
    match model {
        AnyModel::OpenRouter(m, _) => run_summarizer(m, prompt, preamble, max_output_tokens).await,
        AnyModel::OpenAI(m) => match m {
            OpenAiModel::Responses(m) => {
                run_summarizer(m, prompt, preamble, max_output_tokens).await
            }
            OpenAiModel::Completions(m) => {
                run_summarizer(m, prompt, preamble, max_output_tokens).await
            }
        },
        AnyModel::Anthropic(m) => run_summarizer(m, prompt, preamble, max_output_tokens).await,
        AnyModel::Gemini(m) => run_summarizer(m, prompt, preamble, max_output_tokens).await,
        AnyModel::Ollama(m) => run_summarizer(m, prompt, preamble, max_output_tokens).await,
    }
}

async fn run_summarizer<M>(
    model: M,
    prompt: String,
    preamble: String,
    max_output_tokens: u64,
) -> anyhow::Result<String>
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let agent = rig::agent::AgentBuilder::new(model)
        .preamble(&preamble)
        .max_tokens(max_output_tokens)
        .build();

    let agent_ref = &agent;
    let mut stream = retry::retry_stream_chat(&RetryConfig::default(), move || {
        let p = prompt.clone();
        async move {
            agent_ref
                .stream_chat(p, Vec::<Message>::new())
                .max_turns(1)
                .await
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("Compression failed: {}", e))?;

    let mut response = String::new();
    use futures::StreamExt;
    while let Some(item) = stream.next().await {
        match item {
            Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                rig::streaming::StreamedAssistantContent::Text(text),
            )) => response.push_str(&text.text),
            Ok(rig::agent::MultiTurnStreamItem::FinalResponse(res)) => {
                response = res.output.to_string();
                break;
            }
            Err(e) => return Err(anyhow::anyhow!("Compression failed: {}", e)),
            _ => {}
        }
    }

    if response.is_empty() {
        anyhow::bail!("Compression returned empty response");
    }

    Ok(response)
}

fn summarizer_preamble() -> String {
    let mut preamble = prompt::COMPACTION_SYSTEM_PROMPT.to_string();
    if let Some(s) = crate::session::storage::load_suffix() {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(&s);
    }
    preamble
}

pub(crate) fn serialize_conversation(messages: &[SessionMessage]) -> String {
    let mut result = String::new();
    for msg in messages {
        let role_tag = match msg.role {
            crate::session::MessageRole::User => "user",
            crate::session::MessageRole::Assistant => "assistant",
            crate::session::MessageRole::System => "system",
            crate::session::MessageRole::ToolCall => "tool_call",
            crate::session::MessageRole::ToolResult => "tool_result",
            crate::session::MessageRole::SubagentToolCall => "subagent_tool_call",
        };
        // XML-based format prevents injection: untrusted content cannot escape
        // the <message> tag, and the role attribute is never injectable.
        result.push_str(&format!(
            "<message role=\"{}\">\n{}\n</message>\n",
            role_tag, msg.content
        ));
    }
    result
}

pub enum AnyModel {
    /// The second field carries provider-specific extra body params. For
    /// `anthropic/*` models routed via OpenRouter it pins `provider.order` to
    /// the Anthropic direct route, the only route that honors `cache_control`
    /// breakpoints (Bedrock/Vertex silently drop them). `None` for every other
    /// OpenRouter model, which caches automatically and needs no routing.
    OpenRouter(
        openrouter::completion::CompletionModel,
        Option<serde_json::Value>,
    ),
    OpenAI(OpenAiModel),
    Anthropic(anthropic::completion::CompletionModel),
    Gemini(gemini::completion::CompletionModel),
    Ollama(ollama::CompletionModel),
}

#[derive(Clone)]
pub enum AnyAgentInner {
    OpenRouter(Agent<openrouter::completion::CompletionModel>),
    OpenAI(OpenAiAgent),
    Anthropic(Agent<anthropic::completion::CompletionModel>),
    Gemini(Agent<gemini::completion::CompletionModel>),
    Ollama(Agent<ollama::CompletionModel>),
}

#[derive(Clone)]
pub struct AnyAgent {
    inner: AnyAgentInner,
    /// Cumulative input+output token cap enforced per spawned turn. `None`
    /// disables it. Kept here, not on the rig agent, because the rig agent's
    /// `max_tokens` is the per-response output cap sent to the provider — a
    /// different unit entirely.
    turn_token_budget: Option<u64>,
    #[cfg(feature = "skills")]
    skills: Option<std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>>,
    #[cfg(feature = "skills")]
    turn_gate: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl AnyAgent {
    pub(crate) fn without_skills(inner: AnyAgentInner) -> Self {
        Self {
            inner,
            turn_token_budget: None,
            #[cfg(feature = "skills")]
            skills: None,
            #[cfg(feature = "skills")]
            turn_gate: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub(crate) fn with_turn_token_budget(mut self, budget: Option<u64>) -> Self {
        self.turn_token_budget = budget;
        self
    }

    fn with_runtime(
        inner: AnyAgentInner,
        #[cfg(feature = "skills")] skills: Option<
            std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
        >,
    ) -> Self {
        #[cfg(feature = "skills")]
        let turn_gate = skills
            .as_ref()
            .map(|services| services.turn_gate())
            .unwrap_or_else(|| std::sync::Arc::new(tokio::sync::Mutex::new(())));
        Self {
            inner,
            turn_token_budget: None,
            #[cfg(feature = "skills")]
            skills,
            #[cfg(feature = "skills")]
            turn_gate,
        }
    }
}

/// Synthesizes an `AgentRunner` for a prompt blocked by a `UserPromptSubmit`
/// hook: no model call happens, the block feedback surfaces through the same
/// `AgentEvent::Error` path a real run-time error would use.
#[cfg(feature = "hooks")]
fn spawn_blocked_runner(
    feedback: String,
    work_scope: std::sync::Arc<runner::AgentWorkScope>,
) -> runner::PausedAgentRunner {
    let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(1);
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let cleanup_scope = std::sync::Arc::clone(&work_scope);
    let lifecycle_tx = event_tx.clone();
    let cleanup = runner::AgentRunCleanupGuard::new(cleanup_scope, lifecycle_tx);
    let join = tokio::spawn(async move {
        if start_rx.await.is_ok() {
            let _ = event_tx
                .send(AgentEvent::Error(CompactString::from(feedback)))
                .await;
        }
        drop(event_tx);
        cleanup.settle().await;
    });
    runner::PausedAgentRunner::new(
        AgentRunner {
            event_rx,
            abort_handle: join.abort_handle(),
        },
        start_tx,
        work_scope,
    )
}

impl AnyAgent {
    pub async fn run_print(
        &self,
        prompt: &str,
        pure_stdout: bool,
        retry_config: &RetryConfig,
        // Prior turns from a resumed session; see `runner::run_print`. Empty
        // for a fresh session.
        history: Vec<Message>,
        // `--loop` iteration/active state; see `runner::run_print`. `None`
        // for plain `-p` one-shot runs.
        #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
    ) -> anyhow::Result<(String, rig::completion::Usage, Vec<Message>)> {
        #[cfg(feature = "skills")]
        let _turn_guard = if self.skills.is_some() {
            Some(self.turn_gate.lock().await)
        } else {
            None
        };
        #[cfg(feature = "skills")]
        let prompt = if let Some(skills) = &self.skills {
            skills.prepare_prompt(prompt).await
        } else {
            prompt.to_string()
        };
        #[cfg(not(feature = "skills"))]
        let prompt = prompt.to_string();
        match &self.inner {
            AnyAgentInner::OpenRouter(a) => {
                runner::run_print(
                    a,
                    &prompt,
                    pure_stdout,
                    retry_config,
                    self.turn_token_budget,
                    history,
                    #[cfg(feature = "hooks")]
                    loop_info,
                )
                .await
            }
            AnyAgentInner::OpenAI(a) => match a {
                OpenAiAgent::Responses(a) => {
                    runner::run_print(
                        a,
                        &prompt,
                        pure_stdout,
                        retry_config,
                        self.turn_token_budget,
                        history,
                        #[cfg(feature = "hooks")]
                        loop_info,
                    )
                    .await
                }
                OpenAiAgent::Completions(a) => {
                    runner::run_print(
                        a,
                        &prompt,
                        pure_stdout,
                        retry_config,
                        self.turn_token_budget,
                        history,
                        #[cfg(feature = "hooks")]
                        loop_info,
                    )
                    .await
                }
            },
            AnyAgentInner::Anthropic(a) => {
                runner::run_print(
                    a,
                    &prompt,
                    pure_stdout,
                    retry_config,
                    self.turn_token_budget,
                    history,
                    #[cfg(feature = "hooks")]
                    loop_info,
                )
                .await
            }
            AnyAgentInner::Gemini(a) => {
                runner::run_print(
                    a,
                    &prompt,
                    pure_stdout,
                    retry_config,
                    self.turn_token_budget,
                    history,
                    #[cfg(feature = "hooks")]
                    loop_info,
                )
                .await
            }
            AnyAgentInner::Ollama(a) => {
                runner::run_print(
                    a,
                    &prompt,
                    pure_stdout,
                    retry_config,
                    self.turn_token_budget,
                    history,
                    #[cfg(feature = "hooks")]
                    loop_info,
                )
                .await
            }
        }
    }

    #[cfg(feature = "subagents")]
    pub(crate) async fn run_subagent(
        &self,
        prompt: &str,
        max_turns: usize,
        event_tx: Option<&mpsc::Sender<AgentEvent>>,
        retry_config: &RetryConfig,
        usage_ledger: runner::SharedUsageLedger,
    ) -> runner::SubagentRunOutput {
        #[cfg(feature = "skills")]
        let _turn_guard = if self.skills.is_some() {
            Some(self.turn_gate.lock().await)
        } else {
            None
        };
        #[cfg(feature = "skills")]
        let prompt = if let Some(skills) = &self.skills {
            skills.prepare_prompt(prompt).await
        } else {
            prompt.to_string()
        };
        #[cfg(not(feature = "skills"))]
        let prompt = prompt.to_string();
        match &self.inner {
            AnyAgentInner::OpenRouter(a) => {
                runner::run_subagent(a, &prompt, max_turns, event_tx, retry_config, usage_ledger)
                    .await
            }
            AnyAgentInner::OpenAI(a) => match a {
                OpenAiAgent::Responses(a) => {
                    runner::run_subagent(
                        a,
                        &prompt,
                        max_turns,
                        event_tx,
                        retry_config,
                        usage_ledger,
                    )
                    .await
                }
                OpenAiAgent::Completions(a) => {
                    runner::run_subagent(
                        a,
                        &prompt,
                        max_turns,
                        event_tx,
                        retry_config,
                        usage_ledger,
                    )
                    .await
                }
            },
            AnyAgentInner::Anthropic(a) => {
                runner::run_subagent(a, &prompt, max_turns, event_tx, retry_config, usage_ledger)
                    .await
            }
            AnyAgentInner::Gemini(a) => {
                runner::run_subagent(a, &prompt, max_turns, event_tx, retry_config, usage_ledger)
                    .await
            }
            AnyAgentInner::Ollama(a) => {
                runner::run_subagent(a, &prompt, max_turns, event_tx, retry_config, usage_ledger)
                    .await
            }
        }
    }

    /// Async because, under `hooks`, the `UserPromptSubmit` gate must resolve
    /// before spawning: its outcome decides whether the runner spawns at all
    /// (a hook can block the prompt outright) and, if so, with what prompt
    /// (a hook can rewrite it).
    pub async fn spawn_runner(
        self,
        prompt: String,
        history: Vec<Message>,
        retry_config: RetryConfig,
        #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
    ) -> AgentRunner {
        self.spawn_runner_paused(
            prompt,
            history,
            retry_config,
            #[cfg(feature = "hooks")]
            loop_info,
        )
        .await
        .start()
    }

    /// Builds an agent runner behind a start barrier. ACP uses this to publish
    /// cancellation ownership and the abort handle before model/tool execution.
    pub(crate) async fn spawn_runner_paused(
        self,
        prompt: String,
        history: Vec<Message>,
        retry_config: RetryConfig,
        // `--loop` iteration/active state; see `runner::spawn_agent`. `None`
        // outside loop mode.
        #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
    ) -> runner::PausedAgentRunner {
        self.spawn_runner_paused_in_scope(
            prompt,
            history,
            retry_config,
            #[cfg(feature = "hooks")]
            loop_info,
            runner::AgentWorkScope::new(),
        )
        .await
    }

    pub(crate) async fn spawn_runner_paused_in_scope(
        self,
        prompt: String,
        history: Vec<Message>,
        retry_config: RetryConfig,
        #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
        work_scope: std::sync::Arc<runner::AgentWorkScope>,
    ) -> runner::PausedAgentRunner {
        #[cfg(feature = "hooks")]
        let prompt = match work_scope
            .run(crate::extras::hooks::dispatch_user_prompt_submit(prompt))
            .await
        {
            crate::extras::hooks::PromptGate::Blocked(feedback) => {
                return spawn_blocked_runner(feedback, work_scope);
            }
            crate::extras::hooks::PromptGate::Proceed(prompt) => prompt,
        };
        #[cfg(feature = "skills")]
        let turn_guard = if self.skills.is_some() {
            Some(std::sync::Arc::clone(&self.turn_gate).lock_owned().await)
        } else {
            None
        };
        #[cfg(feature = "skills")]
        let prompt = if let Some(skills) = &self.skills {
            work_scope.run(skills.prepare_prompt(&prompt)).await
        } else {
            prompt
        };
        let turn_token_budget = self.turn_token_budget;
        match self.inner {
            AnyAgentInner::OpenRouter(a) => runner::spawn_agent_paused_in_scope(
                a,
                prompt,
                history,
                retry_config,
                turn_token_budget,
                #[cfg(feature = "skills")]
                turn_guard,
                #[cfg(feature = "hooks")]
                loop_info,
                work_scope,
            ),
            AnyAgentInner::OpenAI(a) => match a {
                OpenAiAgent::Responses(a) => runner::spawn_agent_paused_in_scope(
                    a,
                    prompt,
                    history,
                    retry_config,
                    turn_token_budget,
                    #[cfg(feature = "skills")]
                    turn_guard,
                    #[cfg(feature = "hooks")]
                    loop_info,
                    work_scope,
                ),
                OpenAiAgent::Completions(a) => runner::spawn_agent_paused_in_scope(
                    a,
                    prompt,
                    history,
                    retry_config,
                    turn_token_budget,
                    #[cfg(feature = "skills")]
                    turn_guard,
                    #[cfg(feature = "hooks")]
                    loop_info,
                    work_scope,
                ),
            },
            AnyAgentInner::Anthropic(a) => runner::spawn_agent_paused_in_scope(
                a,
                prompt,
                history,
                retry_config,
                turn_token_budget,
                #[cfg(feature = "skills")]
                turn_guard,
                #[cfg(feature = "hooks")]
                loop_info,
                work_scope,
            ),
            AnyAgentInner::Gemini(a) => runner::spawn_agent_paused_in_scope(
                a,
                prompt,
                history,
                retry_config,
                turn_token_budget,
                #[cfg(feature = "skills")]
                turn_guard,
                #[cfg(feature = "hooks")]
                loop_info,
                work_scope,
            ),
            AnyAgentInner::Ollama(a) => runner::spawn_agent_paused_in_scope(
                a,
                prompt,
                history,
                retry_config,
                turn_token_budget,
                #[cfg(feature = "skills")]
                turn_guard,
                #[cfg(feature = "hooks")]
                loop_info,
                work_scope,
            ),
        }
    }

    pub fn spawn_btw(
        self,
        prompt: String,
        history: Vec<Message>,
        event_tx: mpsc::Sender<crate::event::BtwEvent>,
        id: u32,
        retry_config: RetryConfig,
    ) -> crate::agent::runner::BtwRunner {
        match self.inner {
            AnyAgentInner::OpenRouter(a) => {
                runner::spawn_btw(a, prompt, history, event_tx, id, retry_config)
            }
            AnyAgentInner::OpenAI(a) => match a {
                OpenAiAgent::Responses(a) => {
                    runner::spawn_btw(a, prompt, history, event_tx, id, retry_config)
                }
                OpenAiAgent::Completions(a) => {
                    runner::spawn_btw(a, prompt, history, event_tx, id, retry_config)
                }
            },
            AnyAgentInner::Anthropic(a) => {
                runner::spawn_btw(a, prompt, history, event_tx, id, retry_config)
            }
            AnyAgentInner::Gemini(a) => {
                runner::spawn_btw(a, prompt, history, event_tx, id, retry_config)
            }
            AnyAgentInner::Ollama(a) => {
                runner::spawn_btw(a, prompt, history, event_tx, id, retry_config)
            }
        }
    }
}

/// Expands a value that is exactly "${VAR}" to the environment variable's value;
/// any other format is returned as-is. Only whole-string `${VAR}` is supported
/// (the common, safe case) rather than arbitrary interpolation.
pub(crate) fn expand_env(value: &str) -> anyhow::Result<String> {
    if let Some(var) = value.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        std::env::var(var).map_err(|_| {
            anyhow::anyhow!(
                "Environment variable '{var}' (referenced in a custom provider header) is not set"
            )
        })
    } else {
        Ok(value.to_string())
    }
}

/// Builds a shared reqwest client, combining:
/// - `danger_accept_invalid_certs` (from #62; the TLS toggle shared by all providers)
/// - a custom provider's `headers` (values support `${ENV_VAR}` expansion) and `timeout_secs`
///
/// When the provider is not custom (`custom == None`) and TLS is not disabled,
/// the resulting client is equivalent to `reqwest::Client::default()`, so the
/// behavior of existing providers is unchanged.
pub(crate) fn build_http_client(
    provider_name: &str,
    danger_accept_invalid_certs: bool,
    custom: Option<&CustomProviderConfig>,
    base_url: Option<&str>,
) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if is_localhost(base_url) {
        // Disable connection pooling for local LLM servers (notably
        // llama.cpp's cpp-httplib) which close idle keep-alive
        // connections far faster than reqwest's default 90s
        // pool_idle_timeout, leaving stale half-closed sockets.
        builder = builder.pool_max_idle_per_host(0);
    }

    if let Some(cfg) = custom {
        if !cfg.headers.is_empty() {
            let mut headers = HeaderMap::new();
            for (name, raw_value) in &cfg.headers {
                let value = expand_env(raw_value)?;
                let header_name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| anyhow::anyhow!("Invalid header name '{name}': {e}"))?;
                let header_value = HeaderValue::from_str(&value)
                    .map_err(|e| anyhow::anyhow!("Invalid value for header '{name}': {e}"))?;
                headers.insert(header_name, header_value);
            }
            builder = builder.default_headers(headers);
        }
        if let Some(secs) = cfg.timeout_secs {
            builder = builder.timeout(Duration::from_secs(secs));
        }
    }

    if danger_accept_invalid_certs {
        tracing::warn!(
            "TLS certificate verification DISABLED for provider '{}' \
             (danger_accept_invalid_certs = true). Connections are vulnerable to MITM.",
            provider_name
        );
        builder = builder.danger_accept_invalid_certs(true);
    }

    builder.build().map_err(Into::into)
}

fn is_localhost(url: Option<&str>) -> bool {
    url.is_some_and(|u| {
        u.starts_with("http://localhost")
            || u.starts_with("http://127.")
            || u.starts_with("http://[::1]")
    })
}

/// Determines which API style the OpenAI family should use:
/// if `api_style` is set explicitly, honor it; otherwise default to Completions
/// when a base_url is present (i.e. a compatible gateway) and Responses when it
/// is absent (i.e. real api.openai.com).
pub(crate) fn resolve_api_style(
    base_url: Option<&str>,
    custom: Option<&CustomProviderConfig>,
) -> ApiStyle {
    custom.and_then(|c| c.api_style).unwrap_or({
        if base_url.is_some() {
            ApiStyle::Completions
        } else {
            ApiStyle::Responses
        }
    })
}

/// Builds an OpenAI-family client (Responses or Completions) using the
/// already-constructed shared http_client.
fn build_openai_client(
    key: &str,
    base_url: Option<&str>,
    custom: Option<&CustomProviderConfig>,
    http_client: reqwest::Client,
) -> anyhow::Result<OpenAiClient> {
    let style = resolve_api_style(base_url, custom);

    match style {
        ApiStyle::Responses => {
            let client = match base_url {
                Some(u) => openai::Client::builder()
                    .api_key(key)
                    .base_url(u)
                    .http_client(http_client)
                    .build()?,
                None => openai::Client::builder()
                    .api_key(key)
                    .http_client(http_client)
                    .build()?,
            };
            Ok(OpenAiClient::Responses(client))
        }
        ApiStyle::Completions => {
            let client = match base_url {
                Some(u) => openai::CompletionsClient::builder()
                    .api_key(key)
                    .base_url(u)
                    .http_client(http_client)
                    .build()?,
                None => openai::CompletionsClient::builder()
                    .api_key(key)
                    .http_client(http_client)
                    .build()?,
            };
            Ok(OpenAiClient::Completions(client))
        }
    }
}

pub fn create_client(
    provider_name: &str,
    api_key: Option<&str>,
    custom_providers: &HashMap<String, CustomProviderConfig>,
    config_api_keys: Option<&HashMap<String, String>>,
) -> anyhow::Result<AnyClient> {
    let config = resolve_provider_config(provider_name, custom_providers)?;
    let base_url = resolve_base_url(&config);

    let resolver = AuthResolver::new(config.kind)
        .with_cli_key(api_key)
        .with_env_override(config.api_key_env.as_deref())
        .with_config_keys(config_api_keys)
        .with_custom_provider_name(Some(provider_name));
    let key = resolver.resolve()?;

    match config.kind {
        ProviderKind::OpenAI => {
            let custom = custom_providers.get(provider_name);
            let http_client = build_http_client(
                provider_name,
                config.danger_accept_invalid_certs,
                custom,
                base_url.as_deref(),
            )?;
            Ok(AnyClient::OpenAI(build_openai_client(
                &key,
                base_url.as_deref(),
                custom,
                http_client,
            )?))
        }
        ProviderKind::Anthropic => build_anthropic_client(&key, base_url.as_deref()),
        ProviderKind::Gemini => build_gemini_client(&key, base_url.as_deref()),
        ProviderKind::Ollama => build_ollama_client(&key, base_url.as_deref()),
        ProviderKind::OpenRouter => build_openrouter_client(&key, base_url.as_deref()),
    }
}

macro_rules! build_provider_client {
    ($client_ty:ty, $variant:ident, $key_expr:expr, $base_url:expr) => {{
        let key = $key_expr;
        let builder = match $base_url {
            Some(u) => <$client_ty>::builder().api_key(key).base_url(u),
            None => <$client_ty>::builder().api_key(key),
        };
        Ok(AnyClient::$variant(builder.build()?))
    }};
}

fn build_anthropic_client(key: &str, base_url: Option<&str>) -> anyhow::Result<AnyClient> {
    build_provider_client!(anthropic::Client, Anthropic, key, base_url)
}

fn build_gemini_client(key: &str, base_url: Option<&str>) -> anyhow::Result<AnyClient> {
    build_provider_client!(gemini::Client, Gemini, key, base_url)
}

fn build_ollama_client(key: &str, base_url: Option<&str>) -> anyhow::Result<AnyClient> {
    build_provider_client!(
        ollama::Client,
        Ollama,
        ollama::OllamaApiKey::from(key),
        base_url
    )
}

fn build_openrouter_client(key: &str, base_url: Option<&str>) -> anyhow::Result<AnyClient> {
    // Expanded from `build_provider_client!` so we can chain OpenRouter's
    // builder-only app-identity calls: these set `X-OpenRouter-Title` /
    // `HTTP-Referer` / `X-OpenRouter-Categories` so mini-agent's traffic is
    // attributed in OpenRouter's dashboards instead of showing up anonymously.
    let builder = match base_url {
        Some(u) => openrouter::Client::builder().api_key(key).base_url(u),
        None => openrouter::Client::builder().api_key(key),
    };
    let builder = builder
        .with_app_identity(crate::product::PUBLIC_NAME, crate::product::REPOSITORY_URL)
        .with_app_categories(&["cli-agent", "coding"]);
    Ok(AnyClient::OpenRouter(builder.build()?))
}

/// Builds an OpenAiModel (Responses / Completions) into the matching OpenAiAgent.
#[allow(clippy::too_many_arguments)]
async fn build_openai_agent(
    model: OpenAiModel,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    sandbox: Sandbox,
    read_tracker: crate::agent::tools::ReadTracker,
    reasoning_enabled: bool,
    temperature: Option<f64>,
    extra_body: Option<serde_json::Value>,
    #[cfg(feature = "js")]
    js_worker_containment_status: crate::sandbox::worker::WorkerContainmentStatus,
    #[cfg(feature = "skills")] skill_services: Option<
        std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
) -> OpenAiAgent {
    match model {
        OpenAiModel::Responses(m) => OpenAiAgent::Responses(
            builder::build_agent_inner(
                m,
                cli,
                cfg,
                context,
                workspace.clone(),
                permission,
                ask_tx,
                sandbox,
                read_tracker,
                reasoning_enabled,
                temperature,
                extra_body,
                #[cfg(feature = "js")]
                js_worker_containment_status.clone(),
                #[cfg(feature = "skills")]
                skill_services.clone(),
                #[cfg(feature = "mcp")]
                mcp_manager,
            )
            .await,
        ),
        OpenAiModel::Completions(m) => OpenAiAgent::Completions(
            builder::build_agent_inner(
                m,
                cli,
                cfg,
                context,
                workspace,
                permission,
                ask_tx,
                sandbox,
                read_tracker,
                reasoning_enabled,
                temperature,
                extra_body,
                #[cfg(feature = "js")]
                js_worker_containment_status,
                #[cfg(feature = "skills")]
                skill_services,
                #[cfg(feature = "mcp")]
                mcp_manager,
            )
            .await,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn build_agent_in_workspace(
    model: AnyModel,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    sandbox: Sandbox,
    read_tracker: crate::agent::tools::ReadTracker,
    reasoning_enabled: bool,
    temperature: Option<f64>,
    extra_body: Option<serde_json::Value>,
    #[cfg(feature = "skills")] skill_service_owner: std::sync::Arc<
        crate::extras::js::skills::session::SkillServiceOwner,
    >,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
) -> AnyAgent {
    #[cfg(feature = "js")]
    let js_tool_eligible = cli.tool_is_eligible(cfg, "js");
    #[cfg(feature = "js")]
    let js_worker_containment_status =
        resolve_js_worker_containment(js_tool_eligible, crate::sandbox::worker::containment_status);
    #[cfg(feature = "skills")]
    let skills = resolve_skill_services(
        js_tool_eligible,
        &js_worker_containment_status,
        &skill_service_owner,
        &workspace,
        cfg.embedding.clone(),
    )
    .await;

    let inner = match model {
        AnyModel::OpenRouter(m, routing) => AnyAgentInner::OpenRouter(
            builder::build_agent_inner(
                m,
                cli,
                cfg,
                context,
                workspace.clone(),
                permission,
                ask_tx,
                sandbox.clone(),
                read_tracker,
                reasoning_enabled,
                temperature,
                merge_extra_body(routing, extra_body),
                #[cfg(feature = "js")]
                js_worker_containment_status.clone(),
                #[cfg(feature = "skills")]
                skills.clone(),
                #[cfg(feature = "mcp")]
                mcp_manager,
            )
            .await,
        ),
        AnyModel::OpenAI(m) => AnyAgentInner::OpenAI(
            build_openai_agent(
                m,
                cli,
                cfg,
                context,
                workspace.clone(),
                permission,
                ask_tx,
                sandbox.clone(),
                read_tracker,
                reasoning_enabled,
                temperature,
                extra_body,
                #[cfg(feature = "js")]
                js_worker_containment_status.clone(),
                #[cfg(feature = "skills")]
                skills.clone(),
                #[cfg(feature = "mcp")]
                mcp_manager,
            )
            .await,
        ),
        AnyModel::Anthropic(m) => AnyAgentInner::Anthropic(
            builder::build_agent_inner(
                m,
                cli,
                cfg,
                context,
                workspace.clone(),
                permission,
                ask_tx,
                sandbox.clone(),
                read_tracker,
                reasoning_enabled,
                temperature,
                extra_body,
                #[cfg(feature = "js")]
                js_worker_containment_status.clone(),
                #[cfg(feature = "skills")]
                skills.clone(),
                #[cfg(feature = "mcp")]
                mcp_manager,
            )
            .await,
        ),
        AnyModel::Gemini(m) => AnyAgentInner::Gemini(
            builder::build_agent_inner(
                m,
                cli,
                cfg,
                context,
                workspace.clone(),
                permission,
                ask_tx,
                sandbox.clone(),
                read_tracker,
                reasoning_enabled,
                temperature,
                extra_body,
                #[cfg(feature = "js")]
                js_worker_containment_status.clone(),
                #[cfg(feature = "skills")]
                skills.clone(),
                #[cfg(feature = "mcp")]
                mcp_manager,
            )
            .await,
        ),
        AnyModel::Ollama(m) => AnyAgentInner::Ollama(
            builder::build_agent_inner(
                m,
                cli,
                cfg,
                context,
                workspace,
                permission,
                ask_tx,
                sandbox,
                read_tracker,
                reasoning_enabled,
                temperature,
                extra_body,
                #[cfg(feature = "js")]
                js_worker_containment_status,
                #[cfg(feature = "skills")]
                skills.clone(),
                #[cfg(feature = "mcp")]
                mcp_manager,
            )
            .await,
        ),
    };
    AnyAgent::with_runtime(
        inner,
        #[cfg(feature = "skills")]
        skills,
    )
    .with_turn_token_budget(cfg.resolve_turn_token_budget())
}

#[cfg(feature = "skills")]
async fn resolve_skill_services(
    eligible: bool,
    containment: &crate::sandbox::worker::WorkerContainmentStatus,
    owner: &std::sync::Arc<crate::extras::js::skills::session::SkillServiceOwner>,
    workspace: &std::sync::Arc<crate::paths::WorkspaceBinding>,
    embedding: Option<crate::config::EmbeddingConfig>,
) -> Option<std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>> {
    if !eligible
        || !matches!(
            containment,
            crate::sandbox::worker::WorkerContainmentStatus::Available { .. }
        )
    {
        return None;
    }
    owner.resolve(workspace, embedding).await
}

#[cfg(feature = "js")]
fn resolve_js_worker_containment(
    eligible: bool,
    probe: impl FnOnce() -> crate::sandbox::worker::WorkerContainmentStatus,
) -> crate::sandbox::worker::WorkerContainmentStatus {
    if eligible {
        probe()
    } else {
        crate::sandbox::worker::WorkerContainmentStatus::Unavailable {
            backend: crate::sandbox::worker::WorkerBackend::for_current_platform(),
            assurance: crate::sandbox::worker::WorkerContainmentAssurance::Enforced,
            reason: "JavaScript tool was not requested".to_string(),
        }
    }
}

#[cfg(all(test, feature = "js"))]
mod startup_capability_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::resolve_js_worker_containment;
    use crate::sandbox::worker::{
        WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus,
    };

    #[test]
    fn ineligible_javascript_capability_performs_no_worker_probe() {
        let calls = AtomicUsize::new(0);
        let containment = resolve_js_worker_containment(false, || {
            calls.fetch_add(1, Ordering::SeqCst);
            WorkerContainmentStatus::Available {
                backend: WorkerBackend::for_current_platform(),
                assurance: WorkerContainmentAssurance::Enforced,
            }
        });

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            containment,
            WorkerContainmentStatus::Unavailable { .. }
        ));
    }

    #[test]
    fn eligible_javascript_capability_probes_once() {
        let calls = AtomicUsize::new(0);
        let containment = resolve_js_worker_containment(true, || {
            calls.fetch_add(1, Ordering::SeqCst);
            WorkerContainmentStatus::Available {
                backend: WorkerBackend::for_current_platform(),
                assurance: WorkerContainmentAssurance::Enforced,
            }
        });

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            containment,
            WorkerContainmentStatus::Available { .. }
        ));
    }

    #[cfg(feature = "skills")]
    #[tokio::test]
    async fn ineligible_or_uncontained_javascript_initializes_no_skill_services() {
        let workspace_root = std::env::temp_dir().join(format!(
            "mini-agent-skill-services-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace_root).unwrap();
        let workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(&workspace_root).unwrap());
        let owner =
            std::sync::Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new());
        let available = WorkerContainmentStatus::Available {
            backend: WorkerBackend::for_current_platform(),
            assurance: WorkerContainmentAssurance::Enforced,
        };
        let unavailable = WorkerContainmentStatus::Unavailable {
            backend: WorkerBackend::for_current_platform(),
            assurance: WorkerContainmentAssurance::Enforced,
            reason: "test unavailable".to_string(),
        };

        assert!(
            super::resolve_skill_services(false, &available, &owner, &workspace, None)
                .await
                .is_none()
        );
        assert!(
            super::resolve_skill_services(true, &unavailable, &owner, &workspace, None)
                .await
                .is_none()
        );
        assert_eq!(owner.initialization_attempts(), 0);
        drop(workspace);
        std::fs::remove_dir_all(workspace_root).unwrap();
    }
}

/// Builds the isolated, tool-less `/btw` agent for the active provider.
#[allow(clippy::too_many_arguments)]
pub fn build_btw_agent(
    model: AnyModel,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    workspace: &std::sync::Arc<crate::paths::WorkspaceBinding>,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    reasoning_enabled: bool,
    temperature: Option<f64>,
    extra_body: Option<serde_json::Value>,
) -> AnyAgent {
    let inner = match model {
        AnyModel::OpenRouter(m, routing) => {
            AnyAgentInner::OpenRouter(builder::build_btw_agent_inner(
                m,
                cli,
                cfg,
                context,
                workspace,
                permission,
                ask_tx,
                reasoning_enabled,
                temperature,
                merge_extra_body(routing, extra_body),
            ))
        }
        AnyModel::OpenAI(m) => AnyAgentInner::OpenAI(match m {
            OpenAiModel::Responses(m) => OpenAiAgent::Responses(builder::build_btw_agent_inner(
                m,
                cli,
                cfg,
                context,
                workspace,
                permission,
                ask_tx,
                reasoning_enabled,
                temperature,
                extra_body,
            )),
            OpenAiModel::Completions(m) => {
                OpenAiAgent::Completions(builder::build_btw_agent_inner(
                    m,
                    cli,
                    cfg,
                    context,
                    workspace,
                    permission,
                    ask_tx,
                    reasoning_enabled,
                    temperature,
                    extra_body,
                ))
            }
        }),
        AnyModel::Anthropic(m) => AnyAgentInner::Anthropic(builder::build_btw_agent_inner(
            m,
            cli,
            cfg,
            context,
            workspace,
            permission,
            ask_tx,
            reasoning_enabled,
            temperature,
            extra_body,
        )),
        AnyModel::Gemini(m) => AnyAgentInner::Gemini(builder::build_btw_agent_inner(
            m,
            cli,
            cfg,
            context,
            workspace,
            permission,
            ask_tx,
            reasoning_enabled,
            temperature,
            extra_body,
        )),
        AnyModel::Ollama(m) => AnyAgentInner::Ollama(builder::build_btw_agent_inner(
            m,
            cli,
            cfg,
            context,
            workspace,
            permission,
            ask_tx,
            reasoning_enabled,
            temperature,
            extra_body,
        )),
    };
    AnyAgent::without_skills(inner)
}

#[cfg(test)]
mod compaction_tests {
    use super::*;
    use crate::session::{MessageRole, SessionMessage};
    use compact_str::CompactString;

    #[test]
    fn serialize_conversation_bounded_preserves_recent_messages() {
        // Create a session with many messages to simulate a long conversation
        let messages: Vec<SessionMessage> = (0..100)
            .map(|i| SessionMessage {
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: CompactString::from(format!(
                    "Message {}: This is test message number {}",
                    i, i
                )),
                estimated_tokens: 10,
                tool_call_id: None,
            })
            .collect();

        // Use a very small budget to force truncation
        let prompt_budget = 1_000; // Very small budget

        let (conversation, messages_included) =
            serialize_conversation_bounded(&messages, prompt_budget)
                .expect("bounded serialization failed");

        // Verify that:
        // 1. We got some messages included
        assert!(
            messages_included > 0,
            "At least one message should be included"
        );
        // 2. We got fewer than the total
        assert!(
            messages_included < messages.len(),
            "Not all messages should be included with small budget"
        );
        // 3. The included messages are the most recent ones
        // (the serialization should preserve the last messages_included messages in order)
        assert!(
            conversation.contains("Message 99"),
            "Most recent messages should be included"
        );
        // 4. The OLDER_HISTORY_OMITTED marker is present since we didn't include all
        assert!(
            conversation.contains(OLDER_HISTORY_OMITTED),
            "Truncation marker should be present when history is omitted"
        );
        // 5. Older messages should not be fully present (though might appear in the marker text)
        let num_message_0_appears = conversation.matches("Message 0:").count();
        assert_eq!(
            num_message_0_appears, 0,
            "Very old messages should not be serialized"
        );
    }

    #[test]
    fn serialize_conversation_bounded_includes_all_when_fits() {
        let messages: Vec<SessionMessage> = (0..3)
            .map(|i| SessionMessage {
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: CompactString::from(format!("Short message {}", i)),
                estimated_tokens: 5,
                tool_call_id: None,
            })
            .collect();

        let prompt_budget = 10_000; // Large budget
        let (conversation, messages_included) =
            serialize_conversation_bounded(&messages, prompt_budget)
                .expect("bounded serialization failed");

        // With a large budget, all messages should fit
        assert_eq!(
            messages_included,
            messages.len(),
            "All messages should be included with large budget"
        );
        // Should not have the omission marker
        assert!(
            !conversation.contains(OLDER_HISTORY_OMITTED),
            "Truncation marker should not be present when all history fits"
        );
    }

    #[test]
    fn serialize_conversation_bounded_always_fits_at_least_one_message() {
        let messages: Vec<SessionMessage> = (0..10)
            .map(|i| SessionMessage {
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: CompactString::from(
                    "X".repeat(500), // Very long message
                ),
                estimated_tokens: 100,
                tool_call_id: None,
            })
            .collect();

        let prompt_budget = 100; // Extremely small budget
        let (conversation, messages_included) =
            serialize_conversation_bounded(&messages, prompt_budget)
                .expect("bounded serialization should not fail even with tiny budget");

        // Even with tiny budget, at least one message should fit
        assert!(
            messages_included >= 1,
            "At least one message should always fit"
        );
        assert!(!conversation.is_empty(), "Conversation should not be empty");
    }
}
