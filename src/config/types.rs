use std::collections::HashMap;

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

/// Which backend the skill library uses to produce embedding vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingBackendKind {
    /// Offline deterministic projection. Builds and runs everywhere with no
    /// model download and no network access. Vectors are stable and
    /// well-formed but carry no semantic meaning, so retrieval quality is not
    /// comparable to a real model. This is the default because it is the only
    /// backend guaranteed to be available.
    #[default]
    Deterministic,
    /// An OpenAI-compatible embeddings HTTP API. Requires `base_url`, `model`,
    /// `api_key_env`, and `dimensions`.
    External,
    /// Local ONNX/BGE inference via `fastembed`. Requires the `skills-embed`
    /// build feature, which does not compile on every host.
    Local,
}

impl std::fmt::Display for EmbeddingBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deterministic => write!(f, "deterministic"),
            Self::External => write!(f, "external"),
            Self::Local => write!(f, "local"),
        }
    }
}

/// Embedding configuration for the skill library (`[embedding]` in config).
///
/// Example:
///
/// ```toml
/// [embedding]
/// backend = "external"
/// base_url = "https://api.openai.com/v1"
/// model = "text-embedding-3-small"
/// api_key_env = "OPENAI_API_KEY"
/// dimensions = 1536
/// timeout_secs = 30
///
/// [embedding.headers]
/// X-Organization = "acme"
/// ```
///
/// The API key itself is never stored in config — only the name of the
/// environment variable holding it, matching [`CustomProviderConfig`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    /// Which backend to use. Defaults to `deterministic`.
    pub backend: EmbeddingBackendKind,
    /// API root for `backend = "external"`. The embeddings path is appended,
    /// so give the root (e.g. `https://api.openai.com/v1`), not the full
    /// endpoint. A trailing slash is tolerated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Model name sent to the external API, or the local model identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<CompactString>,
    /// Name of the environment variable holding the API key. The key is read
    /// at startup and never written back to config or logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<CompactString>,
    /// Vector dimensions the external model returns. Required for `external`
    /// because vectors of a different width must be rejected rather than
    /// silently mixed into an existing index generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<usize>,
    /// Opaque revision tag recorded alongside stored vectors. Change it when
    /// the upstream model changes so old vectors become ineligible instead of
    /// being compared against incompatible new ones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_revision: Option<CompactString>,
    /// Request timeout in seconds. Defaults to 30.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Extra headers sent with every embedding request.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickModelConfig {
    pub provider: CompactString,
    pub model: CompactString,
    #[serde(default)]
    pub input_token_cost: f64,
    #[serde(default)]
    pub output_token_cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    /// Per-model temperature override (0.0–2.0). Takes precedence over the
    /// global `temperature` setting but is overridden by `--temperature`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Provider-specific JSON shallow-merged into the completion request body
    /// (e.g. OpenRouter `plugins` routing presets). Overrides the global
    /// `extra_body` for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Value>,
    /// Per-model context window override. Takes precedence over the static
    /// model catalog but is overridden by the global `context_window` setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

/// Defaults for goal bounds and the judge.
///
/// Every field here only limits how long a goal keeps working or who reviews a
/// completion claim. Nothing in this table can widen what the agent may run,
/// which is why it is a benign project-config key.
#[cfg(feature = "goal")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalConfig {
    /// `auto` (default), `off`, `session`, or a `quick_models` entry name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_active_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_progress_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap_up_max_agent_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reinject_every: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_every: Option<u32>,
}

/// Status-bar statusline layout. Up to 3 lines, each an ordered list of segments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusLineConfig {
    #[serde(default)]
    pub lines: Vec<StatusLineLine>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusLineLine {
    #[serde(default)]
    pub segments: Vec<StatusLineSegment>,
}

/// Icon for a statusline item: `true` uses the item's built-in glyph, or a
/// string sets a custom one (a named icon like `branch`, or a literal glyph).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IconSpec {
    Auto(bool),
    Custom(CompactString),
}

/// One statusline piece. `item` names the element (see `docs/agent/CONFIG.md`).
/// `color`/`bg` are named colors or `#rrggbb`. `text` is the literal for the
/// `separator` item. `left`/`right` are powerline cap glyphs drawn before/after
/// the item. `icon` shows a glyph before the value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusLineSegment {
    pub item: CompactString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<IconSpec>,
    /// Force a numeric item (`tokens_input`, `tokens_output`, `cost`) to show
    /// even when its value is zero (normally hidden).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiStyle {
    Responses,
    Completions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomProviderConfig {
    pub provider_type: CompactString,
    pub base_url: String,
    pub api_key_env: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub danger_accept_invalid_certs: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_style: Option<ApiStyle>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Deadline for establishing the connection, in seconds. Defaults to
    /// `DEFAULT_PROVIDER_CONNECT_TIMEOUT_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u64>,
    /// Deadline between successive reads of a response, in seconds. It resets
    /// on every read, so a long healthy stream is never interrupted, but a peer
    /// that stalls before headers or between events cannot keep a turn alive
    /// forever. Defaults to `DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_idle_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<CompactString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditSystem {
    #[default]
    Similarity,
    Hashedit,
}

impl std::fmt::Display for EditSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditSystem::Similarity => write!(f, "similarity"),
            EditSystem::Hashedit => write!(f, "hashedit"),
        }
    }
}

impl std::str::FromStr for EditSystem {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "similarity" => Ok(EditSystem::Similarity),
            "hashedit" => Ok(EditSystem::Hashedit),
            _ => Err(format!(
                "unknown edit system '{}' (valid: similarity, hashedit)",
                s
            )),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SchemeType {
    #[default]
    Full,
    Ansi,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorsConfig {
    pub chat_background: Option<CompactString>,
    pub input_background: Option<CompactString>,
    pub status_background: Option<CompactString>,
    #[serde(default)]
    pub scheme_type: SchemeType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChainConfig {
    #[serde(rename = "brainstorm-to-plan")]
    pub brainstorm_to_plan: bool,
    #[serde(rename = "plan-to-code")]
    pub plan_to_code: bool,
    #[serde(rename = "code-to-review")]
    pub code_to_review: bool,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            brainstorm_to_plan: true,
            plan_to_code: true,
            code_to_review: false,
        }
    }
}

/// Configuration for LSP (Language Server Protocol) integration. When
/// enabled, language servers are spawned lazily for edited files and
/// diagnostics are fed back to the agent after edits.
#[cfg(feature = "lsp")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LspConfig {
    pub enabled: bool,
    /// Per-server overrides or custom servers, keyed by name. An entry with
    /// the same name as a built-in default replaces it.
    pub servers: HashMap<String, LspServerConfig>,
}

#[cfg(feature = "lsp")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LspServerConfig {
    /// Server binary, resolved via PATH. Empty when the entry only disables
    /// a built-in server.
    pub command: CompactString,
    pub args: Vec<CompactString>,
    /// File extensions this server handles, e.g. [".rs"].
    pub extensions: Vec<CompactString>,
    pub env: HashMap<String, String>,
    /// Parent environment names deliberately delegated after clearing the
    /// ambient process environment. Explicit `env` values always win.
    pub inherit_env: Vec<String>,
    /// Optional workspace-service sandbox backend. Omission is an explicit
    /// trusted-service bypass, not a claim that containment is active.
    pub sandbox: Option<CompactString>,
    /// Network authority requested for the server process.
    pub network: LspNetwork,
    /// Server-specific `initializationOptions` sent during `initialize`.
    pub initialization: Option<serde_json::Value>,
    pub disabled: bool,
}

#[cfg(feature = "lsp")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LspNetwork {
    #[default]
    Inherit,
    Deny,
}

#[cfg(feature = "advisor")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvisorConfig {
    pub enabled: bool,
    pub model: Option<CompactString>,
    pub max_uses: Option<usize>,
    pub human_handoff: bool,
    pub advisor_kilobytes_limit: u32,
}

#[cfg(feature = "advisor")]
impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: Some(CompactString::new("deepseek-v4-pro")),
            max_uses: Some(3),
            human_handoff: true,
            advisor_kilobytes_limit: 256,
        }
    }
}

/// How much internal reasoning a reasoning-capable model should spend.
///
/// Maps to the OpenAI Responses API's `reasoning.effort` and to Chat
/// Completions' top-level `reasoning_effort`. Values match the provider wire
/// spellings exactly, so an unknown value is rejected at config parse time
/// rather than turning every request into a provider error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    /// The provider wire value for this effort level.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            ReasoningEffort::None => "none",
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
        }
    }
}

/// How verbose the provider-rendered reasoning summary should be.
///
/// Maps to the OpenAI Responses API's `reasoning.summary`. There is no Chat
/// Completions equivalent, so this key is ignored on the Completions path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
}

impl ReasoningSummary {
    /// The provider wire value for this summary level.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            ReasoningSummary::Auto => "auto",
            ReasoningSummary::Concise => "concise",
            ReasoningSummary::Detailed => "detailed",
        }
    }
}

/// First-class reasoning controls for the OpenAI family.
///
/// These are typed rather than hand-written `extra_body` JSON because the
/// Responses request body is validated by rig against a closed schema: a
/// misspelled key there is silently dropped and an out-of-range `include`
/// value fails every request.
///
/// # Replay across gateways
///
/// A persisted reasoning item can only be replayed on a later turn if its
/// content travelled back to us in the first place. The Responses API returns
/// that content only when the request asked for it via
/// `include: ["reasoning.encrypted_content"]`, which is why
/// [`ReasoningConfig::encrypted_content`] defaults to on. Without it a
/// persisted item carries nothing but an id, and the id resolves only against
/// server-side state the upstream stored.
///
/// # `store` and `previous_response_id` caveats
///
/// * `store` defaults to **true** at OpenAI, meaning responses are retained
///   server side and reasoning ids resolve on the next turn. A gateway that
///   does not persist state, or that rewrites ids, breaks that continuation —
///   set `store = false` there and rely on encrypted reasoning content
///   instead. `store = false` is also the right setting for zero-retention
///   deployments.
/// * `previous_response_id` is deliberately **not** exposed here. This agent
///   always sends the full input for a turn, so a globally pinned
///   `previous_response_id` would resend the whole conversation *and* ask the
///   provider to prepend a stored one. It is stripped from `extra_body` at
///   config load for the same reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReasoningConfig {
    /// `reasoning.effort` on Responses, `reasoning_effort` on Completions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// `reasoning.summary` on Responses. Ignored on Completions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummary>,
    /// Whether the Responses request asks for `reasoning.encrypted_content`.
    /// Defaults to **on**: it is what makes a persisted reasoning item
    /// replayable on a gateway that does not retain server-side state. Set to
    /// `false` only for an endpoint that rejects the `include` value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<bool>,
    /// Responses `store`. `None` leaves the provider default (true at OpenAI)
    /// untouched. See the type-level caveats before changing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
}

impl ReasoningConfig {
    /// Whether a request should ask for encrypted reasoning content. On by
    /// default, including when no `[reasoning]` table is configured at all.
    pub fn wants_encrypted_content(this: Option<&Self>) -> bool {
        this.and_then(|config| config.encrypted_content)
            .unwrap_or(true)
    }
}
