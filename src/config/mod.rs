pub mod load;
pub mod types;

use std::collections::{BTreeMap, HashMap};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

pub use load::*;
pub use types::*;

use crate::permission::{PermissionConfig, PermissionConfigs};
use crate::retry::RetryConfig;

#[cfg(feature = "mcp")]
use crate::extras::mcp::config::McpServerConfig;

#[cfg(feature = "acp")]
use crate::extras::acp::config::AcpServerConfig;

/// Opaque storage for config values not owned by the running build.
///
/// The wrapper is public only so existing `Config { ..Default::default() }`
/// construction remains possible outside this module. Its map is private,
/// preventing callers from manufacturing conflicts with typed fields.
#[doc(hidden)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PreservedConfig(BTreeMap<String, toml::Value>);

/// Default `max_bash_output_lines` when the config does not set one. Output
/// beyond this many lines keeps its head and tail with an omitted-count marker.
pub const DEFAULT_MAX_BASH_OUTPUT_LINES: u64 = 2000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<CompactString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<CompactString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Cumulative fail-closed token budget for one agentic turn: the sum of
    /// input+output tokens across every completion call the turn makes. `None`
    /// (default) disables the cap — turns are still bounded by
    /// `max_agent_turns`. This is deliberately a separate knob from
    /// `max_tokens`, which is the per-response output cap sent to the
    /// provider; a single multi-tool-call turn legitimately accumulates far
    /// more than one response's worth of prompt tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_token_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Provider-specific JSON shallow-merged into every completion request body
    /// as a global default. A matching `quick_models` entry's `extra_body`
    /// overrides this. Note: body params are provider-specific, so a global
    /// value does not follow model switches — bundle per-`quick_models` when in
    /// doubt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Value>,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_context_files: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_agent_turns: Option<usize>,
    /// Trusted operator command run before a tool-using turn may complete.
    /// Project-local values require the existing sensitive-config approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_command: Option<CompactString>,
    /// Wall-clock bound for one verification attempt. Default: 300 seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_timeout_secs: Option<u64>,
    /// Total verification attempts within one agent turn. Default: 3; capped
    /// at 8 so a persistently failing command cannot loop indefinitely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_text_file_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_read_lines: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bash_output_lines: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_grep_results: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_find_results: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_list_dir_entries: Option<u64>,
    // --- Subagent tool limits (applied when subagents spawn) ---
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_max_read_lines: Option<u64>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_max_grep_results: Option<u64>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_max_find_results: Option<u64>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_max_list_dir_entries: Option<u64>,
    // --- End subagent limits ---
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_enabled: Option<bool>,
    /// Opt-in mid-turn compaction threshold, as a fraction of the context
    /// window (0.0–1.0) of real provider prompt pressure. `None` (default)
    /// disables mid-turn compaction entirely; the agent only compacts between
    /// turns. Honored only when `compact_enabled` is also true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mid_turn_compact_threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub always_show_welcome: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "auto-update-prompts"
    )]
    pub auto_update_prompts: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "auto-update-themes")]
    pub auto_update_themes: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_providers: Option<HashMap<String, types::CustomProviderConfig>>,
    /// Embedding backend for the skill library. Absent means the built-in
    /// offline deterministic backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<types::EmbeddingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "permission-regex")]
    pub permission_regex: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "permission-allow")]
    pub permission_allow: Option<HashMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "permission-ask")]
    pub permission_ask: Option<HashMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "permission-deny")]
    pub permission_deny: Option<HashMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restrictive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_all: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "sandbox-backend")]
    pub sandbox_backend: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "windows-appcontainer-read-roots"
    )]
    pub windows_appcontainer_read_roots: Vec<std::path::PathBuf>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "windows-appcontainer-write-roots"
    )]
    pub windows_appcontainer_write_roots: Vec<std::path::PathBuf>,
    #[cfg(feature = "js")]
    #[serde(skip_serializing_if = "Option::is_none", rename = "js-file-base-dir")]
    pub js_file_base_dir: Option<String>,
    #[cfg(feature = "js")]
    #[serde(skip_serializing_if = "Option::is_none", rename = "js-read-roots")]
    pub js_read_roots: Option<Vec<String>>,
    #[cfg(feature = "js")]
    #[serde(skip_serializing_if = "Option::is_none", rename = "js-write-roots")]
    pub js_write_roots: Option<Vec<String>>,
    #[cfg(feature = "js")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "js-read-unrestricted"
    )]
    pub js_read_unrestricted: Option<bool>,
    #[cfg(feature = "js")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "js-write-unrestricted"
    )]
    pub js_write_unrestricted: Option<bool>,
    #[cfg(feature = "js")]
    #[serde(skip_serializing_if = "Option::is_none", rename = "js-fetch-origins")]
    pub js_fetch_origins: Option<Vec<String>>,
    #[cfg(feature = "js")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "js-fetch-allow-http"
    )]
    pub js_fetch_allow_http: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_all_mcp_calls: Option<bool>,
    /// Bound on one MCP `tools/call` round trip, in seconds. Default: 120.
    #[cfg(feature = "mcp")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_tool_timeout_secs: Option<u64>,
    #[cfg(feature = "mcp")]
    #[serde(skip_serializing_if = "Option::is_none", rename = "enable-exa-mcp")]
    pub enable_exa_mcp: Option<bool>,
    #[cfg(feature = "mcp")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "enable-context7-mcp"
    )]
    pub enable_context7_mcp: Option<bool>,
    #[cfg(feature = "mcp")]
    #[serde(skip_serializing_if = "Option::is_none", rename = "enable-grepapp-mcp")]
    pub enable_grepapp_mcp: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "permission-modes")]
    pub permission_modes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_tool_details: Option<ShowToolDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_reasoning: Option<bool>,
    /// Configurable status-bar (up to 3 lines). When absent, a built-in
    /// default layout is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statusline: Option<types::StatusLineConfig>,
    /// Left padding (columns) for the chat area. Default: 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_left_margin: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_prompt: Option<CompactString>,
    #[cfg(feature = "git-worktree")]
    #[serde(skip_serializing_if = "Option::is_none", rename = "wt-auto-merge")]
    pub wt_auto_merge: Option<bool>,
    #[cfg(feature = "git-worktree")]
    #[serde(skip_serializing_if = "Option::is_none", rename = "wt-base-dir")]
    pub wt_base_dir: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_keys: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_models: Option<HashMap<String, types::QuickModelConfig>>,
    /// Map prompt names to quick-model names. When switching to a prompt,
    /// zerostack looks up the quick model and switches provider+model.
    /// Empty-string values are treated as "no change".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_to_model: Option<HashMap<String, String>>,
    #[cfg(feature = "mcp")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,
    #[cfg(feature = "acp")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_servers: Option<HashMap<String, AcpServerConfig>>,
    #[cfg(feature = "acp")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_host: Option<String>,
    #[cfg(feature = "acp")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_system: Option<types::EditSystem>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_max_turns: Option<usize>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_max_prompts: Option<usize>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_max_concurrency: Option<usize>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_max_output_bytes: Option<usize>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_max_cost_units: Option<u64>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny_repeated_reads: Option<bool>,
    /// Show the session cost in the status bar even when it is $0.0000 (e.g. when
    /// the model has no per-token pricing configured). Default: false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_cost_always: Option<bool>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_enabled: Option<bool>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_model: Option<CompactString>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_provider: Option<CompactString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colors: Option<types::ColorsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain: Option<types::ChainConfig>,
    #[cfg(feature = "lsp")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsp: Option<types::LspConfig>,
    #[cfg(feature = "advisor")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advisor: Option<types::AdvisorConfig>,
    /// Values not owned by this build, including fields behind disabled Cargo
    /// features and fields introduced by newer mini-agent versions.
    ///
    /// This map is private so callers can only mutate owned typed fields.
    /// Deserialization removes owned keys before flattening the remainder,
    /// which makes typed fields authoritative and prevents duplicate-key
    /// conflicts during serialization.
    #[serde(flatten)]
    #[doc(hidden)]
    pub preserved: PreservedConfig,
}

impl Config {
    pub fn custom_providers_map(&self) -> HashMap<String, types::CustomProviderConfig> {
        self.custom_providers.clone().unwrap_or_default()
    }

    /// Whether requests for `provider` go through the Anthropic-native API
    /// route. This is the route that enables prompt caching and reports
    /// `input_tokens` *excluding* cached/cache-creation tokens, so it is the
    /// route whose context accounting must always add the cache fields back in
    /// (see [`Session::real_input_tokens`](crate::session::Session::real_input_tokens)).
    ///
    /// Keyed on the resolved provider *kind*, not the user-facing name: a
    /// custom provider registered under any name but with
    /// `provider_type = "anthropic"` still hits the native route, while
    /// OpenRouter — even when serving a Claude model — normally uses the OpenAI
    /// shape (`input_tokens` already includes cached) and must not. Defensive
    /// normalization of non-native gateways uses their reported total instead.
    pub fn is_anthropic_native(&self, provider: &str) -> bool {
        let kind_name = self
            .custom_providers
            .as_ref()
            .and_then(|m| m.get(provider))
            .map(|c| c.provider_type.as_str())
            .unwrap_or(provider);
        matches!(
            crate::auth::ProviderKind::from_name(kind_name),
            Some(crate::auth::ProviderKind::Anthropic)
        )
    }

    pub fn resolve_context_window(
        &self,
        provider: &str,
        model_id: &str,
        qm: &HashMap<String, types::QuickModelConfig>,
    ) -> u64 {
        if let Some(cw) = self.context_window {
            return cw;
        }
        for qmc in qm.values() {
            if qmc.model.as_str() == model_id
                && let Some(cw) = qmc.context_window
            {
                return cw;
            }
        }
        Self::catalog_context_window(provider, model_id).unwrap_or(128_000)
    }

    /// The model's context window straight from the static catalog, or `None`
    /// when the provider/model is not listed (custom gateways, ollama, or an id
    /// without a `context` entry). Unlike [`resolve_context_window`], this
    /// ignores the config override and the 128k fallback, so callers can tell a
    /// real catalog value apart from the default.
    pub fn catalog_context_window(provider: &str, model_id: &str) -> Option<u64> {
        let entries = crate::models_catalog::catalog_entries(provider)?;
        entries
            .iter()
            .find(|e| e.id == model_id)
            .and_then(|e| e.context_length)
            .map(|cl| cl as u64)
    }

    /// The model's input/output cost (USD per million tokens) straight from
    /// the static catalog, or `None` when the provider/model isn't listed or
    /// carries no baked-in pricing (e.g. OpenRouter, which prices live via
    /// `fetch_openrouter_pricing` instead).
    pub fn catalog_input_output_cost(provider: &str, model_id: &str) -> Option<(f64, f64)> {
        let entries = crate::models_catalog::catalog_entries(provider)?;
        entries
            .iter()
            .find(|e| e.id == model_id)
            .and_then(|e| e.input_price.zip(e.output_price))
    }

    /// Headroom kept free before between-turn compaction triggers. When unset
    /// (globally and on the active quick model), the default scales with the
    /// context window instead of being a fixed constant: a fixed 8k reserve on
    /// a 1M-token model would defer compaction until the window is ~99% full,
    /// leaving the summarizer an impossibly large history to compress in one
    /// call. `window/10` bounded below by the default response cap (16_384, so
    /// one maximal response can never overshoot the window) and above by
    /// `window/2` (so tiny windows keep most of their space usable).
    pub fn resolve_reserve_tokens(
        &self,
        model_id: &str,
        qm: &HashMap<String, types::QuickModelConfig>,
        context_window: u64,
    ) -> u64 {
        if let Some(rt) = self.reserve_tokens {
            return rt;
        }
        for qmc in qm.values() {
            if qmc.model.as_str() == model_id
                && let Some(rt) = qmc.reserve_tokens
            {
                return rt;
            }
        }
        (context_window / 10).max(16_384).min(context_window / 2)
    }

    /// Recent-token budget kept verbatim through a compaction. When unset the
    /// default scales with the context window — keeping only 10k of a 1M-token
    /// conversation would discard far more working context than the window
    /// requires — bounded to `[10_000, 50_000]` and never more than a quarter
    /// of the window.
    pub fn resolve_keep_recent_tokens(&self, context_window: u64) -> u64 {
        if let Some(kr) = self.keep_recent_tokens {
            return kr;
        }
        if context_window == 0 {
            // Window 0 disables auto-compaction; keep the historical default
            // for a manual /compress rather than degenerating to "keep
            // nothing".
            return 10_000;
        }
        (context_window / 20)
            .clamp(10_000, 50_000)
            .min(context_window / 4)
    }

    /// Cumulative per-turn input+output token cap. `None` (default) means no
    /// cap; see the field docs. Never derived from `max_tokens`, which caps a
    /// single response, not a turn.
    pub fn resolve_turn_token_budget(&self) -> Option<u64> {
        self.turn_token_budget
    }

    pub fn resolve_verify_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.verify_timeout_secs.unwrap_or(300).clamp(1, 3_600))
    }

    pub fn resolve_verify_max_attempts(&self) -> u32 {
        self.verify_max_attempts.unwrap_or(3).clamp(1, 8)
    }

    pub fn resolve_chat_left_margin(&self) -> u16 {
        self.chat_left_margin.unwrap_or(0)
    }

    /// Resolves temperature: CLI `--temperature` > quick-model `temperature` >
    /// global `temperature`. Returns `None` when no temperature is configured.
    pub fn resolve_temperature(
        &self,
        cli: &crate::cli::Cli,
        model_id: &str,
        qm: &HashMap<String, types::QuickModelConfig>,
    ) -> Option<f64> {
        if let Some(temp) = cli.temperature {
            return Some(temp.clamp(0.0, 2.0));
        }
        for qmc in qm.values() {
            if qmc.model.as_str() == model_id
                && let Some(temp) = qmc.temperature
            {
                return Some(temp.clamp(0.0, 2.0));
            }
        }
        self.temperature.map(|t| t.clamp(0.0, 2.0))
    }

    /// Resolves provider-specific request-body params: quick-model `extra_body` >
    /// global `extra_body`. Returns `None` when neither is configured. The
    /// resolved value is shallow-merged into the completion request body at
    /// agent-build time.
    pub fn resolve_extra_body(
        &self,
        model_id: &str,
        qm: &HashMap<String, types::QuickModelConfig>,
    ) -> Option<serde_json::Value> {
        for qmc in qm.values() {
            if qmc.model.as_str() == model_id
                && let Some(eb) = &qmc.extra_body
            {
                return Some(eb.clone());
            }
        }
        self.extra_body.clone()
    }

    pub fn resolve_compact_enabled(&self) -> bool {
        self.compact_enabled.unwrap_or(false)
    }

    /// Mid-turn compaction pressure threshold as a fraction of the context
    /// window. Unlike the other resolvers this one substitutes **no** enabling
    /// default: `None` means the mid-turn trigger never fires (preserving the
    /// historical between-turn-only behavior). Values outside `(0.0, 1.0]` are
    /// treated as unset; [`load`](crate::config::load) warns about such values
    /// once at startup, since this resolver runs in the per-call hot path and
    /// must not log. The caller must additionally check
    /// [`resolve_compact_enabled`](Self::resolve_compact_enabled), which is the
    /// master switch for all compaction.
    pub fn resolve_mid_turn_compact_threshold(&self) -> Option<f64> {
        match self.mid_turn_compact_threshold {
            Some(t) if t > 0.0 && t <= 1.0 => Some(t),
            _ => None,
        }
    }

    pub fn resolve_max_read_lines(&self) -> u64 {
        self.max_read_lines.unwrap_or(2000)
    }

    /// Line cap applied to shell tool output returned to the model.
    ///
    /// Defaults to [`DEFAULT_MAX_BASH_OUTPUT_LINES`] so successful output is
    /// bounded even when the config never mentions it; an explicit value
    /// overrides the default and an explicit `0` disables line truncation
    /// (`None`) while the byte-level command limits still apply.
    pub fn resolve_max_bash_output_lines(&self) -> Option<u64> {
        match self.max_bash_output_lines {
            Some(0) => None,
            Some(lines) => Some(lines),
            None => Some(DEFAULT_MAX_BASH_OUTPUT_LINES),
        }
    }

    /// LSP configuration, `Some` only when an `[lsp]` table exists with
    /// `enabled = true`.
    #[cfg(feature = "lsp")]
    pub fn resolve_lsp(&self) -> Option<&types::LspConfig> {
        self.lsp.as_ref().filter(|l| l.enabled)
    }

    pub fn resolve_max_grep_results(&self) -> u64 {
        self.max_grep_results.unwrap_or(150)
    }

    pub fn resolve_max_find_results(&self) -> u64 {
        self.max_find_results.unwrap_or(150)
    }

    pub fn resolve_max_list_dir_entries(&self) -> Option<u64> {
        self.max_list_dir_entries.or(Some(150))
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_subagent_max_read_lines(&self) -> u64 {
        self.subagent_max_read_lines.unwrap_or(2000)
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_subagent_max_grep_results(&self) -> u64 {
        self.subagent_max_grep_results.unwrap_or(200)
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_subagent_max_find_results(&self) -> u64 {
        self.subagent_max_find_results.unwrap_or(200)
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_subagent_max_list_dir_entries(&self) -> Option<u64> {
        self.subagent_max_list_dir_entries
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_max_prompts(&self) -> usize {
        self.task_max_prompts.unwrap_or(8)
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_max_concurrency(&self) -> usize {
        self.task_max_concurrency.unwrap_or(4)
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_max_output_bytes(&self) -> usize {
        self.task_max_output_bytes.unwrap_or(256 * 1024)
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_max_cost_units(&self) -> u64 {
        self.task_max_cost_units.unwrap_or(500_000)
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_timeout_secs(&self) -> u64 {
        self.task_timeout_secs.unwrap_or(300)
    }

    pub fn resolve_always_show_welcome(&self) -> bool {
        self.always_show_welcome.unwrap_or(false)
    }

    pub fn resolve_show_reasoning(&self) -> bool {
        self.show_reasoning.unwrap_or(false)
    }

    pub fn resolve_auto_update_prompts(&self) -> Option<bool> {
        self.auto_update_prompts
    }

    pub fn resolve_auto_update_themes(&self) -> Option<bool> {
        self.auto_update_themes
    }

    pub fn resolve_show_cost_always(&self) -> bool {
        self.show_cost_always.unwrap_or(false)
    }

    /// Look up the quick-model name associated with a prompt in
    /// `[prompt_to_model]`. Returns `None` when the prompt is not mapped or
    /// the value is an empty string (which means "no change").
    pub fn resolve_prompt_model(&self, prompt_name: &str) -> Option<&str> {
        let map = self.prompt_to_model.as_ref()?;
        let val = map.get(prompt_name)?;
        if val.is_empty() {
            None
        } else {
            Some(val.as_str())
        }
    }

    #[cfg(feature = "mcp")]
    pub fn resolve_enable_exa_mcp(&self) -> bool {
        self.enable_exa_mcp.unwrap_or(true)
    }

    #[cfg(feature = "mcp")]
    pub fn resolve_enable_context7_mcp(&self) -> bool {
        self.enable_context7_mcp.unwrap_or(false)
    }

    #[cfg(feature = "mcp")]
    pub fn resolve_enable_grepapp_mcp(&self) -> bool {
        self.enable_grepapp_mcp.unwrap_or(false)
    }

    pub fn build_permission_config(&self) -> anyhow::Result<PermissionConfigs> {
        fn parse_field(
            field: &str,
            value: Option<&serde_json::Value>,
        ) -> anyhow::Result<PermissionConfig> {
            let Some(value) = value else {
                return Ok(PermissionConfig::default());
            };

            fn validate_action(
                field: &str,
                path: &str,
                value: &serde_json::Value,
            ) -> anyhow::Result<()> {
                match value.as_str() {
                    Some("allow" | "ask" | "deny") => Ok(()),
                    _ => anyhow::bail!(
                        "invalid `{field}` configuration at `{path}`: expected `allow`, `ask`, or `deny`"
                    ),
                }
            }

            let object = value.as_object().ok_or_else(|| {
                anyhow::anyhow!("invalid `{field}` configuration: expected an object")
            })?;
            for (tool, configured) in object {
                if matches!(tool.as_str(), "*" | "doom_loop") {
                    validate_action(field, tool, configured)?;
                } else if tool == "external_directory" {
                    let patterns = configured.as_object().ok_or_else(|| {
                        anyhow::anyhow!(
                            "invalid `{field}` configuration at `external_directory`: expected a pattern object"
                        )
                    })?;
                    for (pattern, action) in patterns {
                        validate_action(field, &format!("external_directory.{pattern}"), action)?;
                    }
                } else if crate::permission::is_configurable_tool_name(tool) {
                    if configured.is_string() {
                        validate_action(field, tool, configured)?;
                    } else {
                        let patterns = configured.as_object().ok_or_else(|| {
                            anyhow::anyhow!(
                                "invalid `{field}` configuration at `{tool}`: expected an action or pattern object"
                            )
                        })?;
                        for (pattern, action) in patterns {
                            validate_action(field, &format!("{tool}.{pattern}"), action)?;
                        }
                    }
                } else {
                    anyhow::bail!(
                        "invalid `{field}` configuration at `{tool}`: unsupported permission tool"
                    );
                }
            }

            let encoded = serde_json::to_vec(value)?;
            let mut deserializer = serde_json::Deserializer::from_slice(&encoded);
            serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
                anyhow::anyhow!(
                    "invalid `{field}` configuration at `{}`: {}",
                    error.path(),
                    error.inner()
                )
            })
        }

        let glob = parse_field("permission", self.permission.as_ref())?;
        let regex = parse_field("permission-regex", self.permission_regex.as_ref())?;

        let mut perm_configs = PermissionConfigs { glob, regex };

        fn validate_entries(
            field: &str,
            entries: &Option<HashMap<String, Vec<String>>>,
        ) -> anyhow::Result<()> {
            if let Some(entries) = entries {
                let mut tools: Vec<_> = entries.keys().collect();
                tools.sort_unstable();
                for tool in tools {
                    if !crate::permission::is_configurable_tool_name(tool) {
                        anyhow::bail!(
                            "invalid `{field}` configuration at `{tool}`: unsupported permission tool"
                        );
                    }
                }
            }
            Ok(())
        }

        validate_entries("permission-allow", &self.permission_allow)?;
        validate_entries("permission-ask", &self.permission_ask)?;
        validate_entries("permission-deny", &self.permission_deny)?;

        if let Some(allow) = &self.permission_allow {
            perm_configs.glob.allow_entries = Some(allow.clone());
        }
        if let Some(ask) = &self.permission_ask {
            perm_configs.glob.ask_entries = Some(ask.clone());
        }
        if let Some(deny) = &self.permission_deny {
            perm_configs.glob.deny_entries = Some(deny.clone());
        }

        Ok(perm_configs)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ShowToolDetails {
    Bool(bool),
    Lines(usize),
}

impl Default for ShowToolDetails {
    fn default() -> Self {
        ShowToolDetails::Lines(1)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ResolvedShowToolDetails {
    Off,
    Limited(usize),
    Unlimited,
}

/// Convenience: resolves temperature with all sources (CLI, quick model, global config).
pub fn resolve_temperature(cli: &crate::cli::Cli, cfg: &Config, model_id: &str) -> Option<f64> {
    let qm = quick_models_map(cfg);
    cfg.resolve_temperature(cli, model_id, &qm)
}

/// Convenience: resolves extra body params (quick model, global config).
pub fn resolve_extra_body(cfg: &Config, model_id: &str) -> Option<serde_json::Value> {
    let qm = quick_models_map(cfg);
    cfg.resolve_extra_body(model_id, &qm)
}

impl ShowToolDetails {
    pub fn resolve(&self) -> ResolvedShowToolDetails {
        match self {
            ShowToolDetails::Bool(false) => ResolvedShowToolDetails::Off,
            ShowToolDetails::Bool(true) => ResolvedShowToolDetails::Unlimited,
            ShowToolDetails::Lines(n) => ResolvedShowToolDetails::Limited(*n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_config(map: HashMap<String, String>) -> Config {
        Config {
            prompt_to_model: Some(map),
            ..Default::default()
        }
    }

    #[test]
    fn reserve_tokens_default_scales_with_context_window() {
        let cfg = Config::default();
        let qm = HashMap::new();
        // Small/medium windows keep the response-cap floor.
        assert_eq!(cfg.resolve_reserve_tokens("m", &qm, 128_000), 16_384);
        // Large windows scale to a tenth so compaction is not deferred to 99%.
        assert_eq!(cfg.resolve_reserve_tokens("m", &qm, 1_000_000), 100_000);
        // Tiny windows never reserve more than half the window.
        assert_eq!(cfg.resolve_reserve_tokens("m", &qm, 8_000), 4_000);
        // An explicit config value always wins.
        let explicit = Config {
            reserve_tokens: Some(9_999),
            ..Default::default()
        };
        assert_eq!(explicit.resolve_reserve_tokens("m", &qm, 1_000_000), 9_999);
    }

    #[test]
    fn keep_recent_tokens_default_scales_with_context_window() {
        let cfg = Config::default();
        assert_eq!(cfg.resolve_keep_recent_tokens(128_000), 10_000);
        assert_eq!(cfg.resolve_keep_recent_tokens(1_000_000), 50_000);
        // Tiny windows keep at most a quarter of the window.
        assert_eq!(cfg.resolve_keep_recent_tokens(8_000), 2_000);
        // Window 0 (auto-compaction disabled) keeps the historical default.
        assert_eq!(cfg.resolve_keep_recent_tokens(0), 10_000);
        let explicit = Config {
            keep_recent_tokens: Some(1_234),
            ..Default::default()
        };
        assert_eq!(explicit.resolve_keep_recent_tokens(1_000_000), 1_234);
    }

    #[test]
    fn turn_token_budget_is_never_derived_from_max_tokens() {
        let cfg = Config {
            max_tokens: Some(16_384),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_turn_token_budget(), None);
        let explicit = Config {
            turn_token_budget: Some(200_000),
            ..Default::default()
        };
        assert_eq!(explicit.resolve_turn_token_budget(), Some(200_000));
    }

    #[test]
    fn completion_verification_defaults_and_bounds_are_stable() {
        let defaults = Config::default();
        assert_eq!(
            defaults.resolve_verify_timeout(),
            std::time::Duration::from_secs(300)
        );
        assert_eq!(defaults.resolve_verify_max_attempts(), 3);

        let below_minimum = Config {
            verify_timeout_secs: Some(0),
            verify_max_attempts: Some(0),
            ..Default::default()
        };
        assert_eq!(
            below_minimum.resolve_verify_timeout(),
            std::time::Duration::from_secs(1)
        );
        assert_eq!(below_minimum.resolve_verify_max_attempts(), 1);

        let above_maximum = Config {
            verify_timeout_secs: Some(86_400),
            verify_max_attempts: Some(u32::MAX),
            ..Default::default()
        };
        assert_eq!(
            above_maximum.resolve_verify_timeout(),
            std::time::Duration::from_secs(3_600)
        );
        assert_eq!(above_maximum.resolve_verify_max_attempts(), 8);
    }

    #[test]
    fn resolve_prompt_model_returns_value_for_known_key() {
        let mut map = HashMap::new();
        map.insert("plan".to_string(), "glm-52".to_string());
        let cfg = make_config(map);
        assert_eq!(cfg.resolve_prompt_model("plan"), Some("glm-52"));
    }

    #[test]
    fn resolve_prompt_model_returns_none_for_unknown_key() {
        let mut map = HashMap::new();
        map.insert("plan".to_string(), "glm-52".to_string());
        let cfg = make_config(map);
        assert_eq!(cfg.resolve_prompt_model("code"), None);
    }

    #[test]
    fn resolve_prompt_model_returns_none_for_empty_string() {
        let mut map = HashMap::new();
        map.insert("brainstorm".to_string(), "".to_string());
        let cfg = make_config(map);
        assert_eq!(cfg.resolve_prompt_model("brainstorm"), None);
    }

    #[test]
    fn resolve_prompt_model_returns_none_when_map_is_none() {
        let cfg = Config {
            prompt_to_model: None,
            ..Default::default()
        };
        assert_eq!(cfg.resolve_prompt_model("plan"), None);
    }

    #[test]
    fn resolve_prompt_model_returns_none_for_empty_map() {
        let cfg = make_config(HashMap::new());
        assert_eq!(cfg.resolve_prompt_model("plan"), None);
    }

    #[test]
    fn toml_deserializes_prompt_to_model() {
        let toml_str = r#"
prompt_to_model = { plan = "glm-52", code = "deepseek-v4-pro", empty_val = "" }
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.resolve_prompt_model("plan"), Some("glm-52"));
        assert_eq!(cfg.resolve_prompt_model("code"), Some("deepseek-v4-pro"));
        assert_eq!(cfg.resolve_prompt_model("empty_val"), None);
        assert_eq!(cfg.resolve_prompt_model("unknown"), None);
    }

    #[test]
    fn toml_prompt_to_model_with_dotted_syntax() {
        let toml_str = r#"
[prompt_to_model]
plan = "glm-52"
code = "deepseek-v4-pro"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.resolve_prompt_model("plan"), Some("glm-52"));
        assert_eq!(cfg.resolve_prompt_model("code"), Some("deepseek-v4-pro"));
    }

    #[test]
    fn default_config_has_no_prompt_to_model() {
        let cfg = Config::default();
        assert_eq!(cfg.resolve_prompt_model("plan"), None);
    }

    #[cfg(feature = "js")]
    #[test]
    fn toml_deserializes_js_file_allow_policy_settings() {
        let toml_str = r#"
js-file-base-dir = "workspace"
js-read-roots = ["src", "docs"]
js-write-roots = ["generated"]
js-read-unrestricted = false
js-write-unrestricted = true
js-fetch-origins = ["https://example.com", "http://public.example:8080"]
js-fetch-allow-http = true
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();

        assert_eq!(cfg.js_file_base_dir.as_deref(), Some("workspace"));
        assert_eq!(
            cfg.js_read_roots,
            Some(vec!["src".to_string(), "docs".to_string()])
        );
        assert_eq!(cfg.js_write_roots, Some(vec!["generated".to_string()]));
        assert_eq!(cfg.js_read_unrestricted, Some(false));
        assert_eq!(cfg.js_write_unrestricted, Some(true));
        assert_eq!(
            cfg.js_fetch_origins,
            Some(vec![
                "https://example.com".to_string(),
                "http://public.example:8080".to_string()
            ])
        );
        assert_eq!(cfg.js_fetch_allow_http, Some(true));
    }
}
