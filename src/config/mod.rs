pub(crate) mod editor;
mod finite;
pub(crate) mod keybindings;
pub(crate) mod keys;
pub(crate) mod layers;
pub mod load;
pub(crate) mod schema;
pub mod types;

use std::collections::{BTreeMap, BTreeSet, HashMap};

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

impl PreservedConfig {
    /// The top-level keys this build did not own when the config was read.
    pub(crate) fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}

/// Default `max_bash_output_lines` when the config does not set one. Output
/// beyond this many lines keeps its head and tail with an omitted-count marker.
pub const DEFAULT_MAX_BASH_OUTPUT_LINES: u64 = 2000;
pub const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 120;
pub const MAX_SHELL_TIMEOUT_SECS: u64 = 600;

/// Captured layer participation for diagnostics. Only catalogue key names
/// are retained; neither values nor unknown user-authored names are copied.
/// A table records its merge chain; individual overwritten dynamic fields
/// are not attributed or copied into diagnostics.
#[derive(Debug, Clone, Default)]
pub struct ConfigProvenance {
    pub(crate) global_file: Option<std::path::PathBuf>,
    #[cfg(feature = "mcp")]
    pub(crate) global_digest: Option<String>,
    pub(crate) global_status: Option<&'static str>,
    pub(crate) global_keys: BTreeSet<&'static str>,
    pub(crate) project_file: Option<std::path::PathBuf>,
    pub(crate) project_keys: BTreeSet<&'static str>,
    pub(crate) project_trust: Option<&'static str>,
    pub(crate) local_file: Option<std::path::PathBuf>,
    pub(crate) local_keys: BTreeSet<&'static str>,
    pub(crate) local_trust: Option<&'static str>,
    pub(crate) sources: BTreeMap<&'static str, BTreeSet<ConfigLayer>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ConfigLayer {
    Global,
    Project,
    Local,
    Cli,
}

impl ConfigLayer {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Local => "project-local",
            Self::Cli => "CLI --config",
        }
    }
}

impl ConfigProvenance {
    pub(crate) fn record(&mut self, value: &toml::Value, layer: ConfigLayer) {
        let Some(table) = value.as_table() else {
            return;
        };
        for (spelling, value) in table {
            let Some(key) = keys::lookup(spelling) else {
                continue;
            };
            if value.as_table().is_some_and(|table| table.is_empty()) {
                continue;
            }
            let sources = self.sources.entry(key.name).or_default();
            if !value.is_table() {
                sources.clear();
            }
            sources.insert(layer);
            for (kind, keys) in [
                (ConfigLayer::Global, &mut self.global_keys),
                (ConfigLayer::Project, &mut self.project_keys),
                (ConfigLayer::Local, &mut self.local_keys),
            ] {
                if layer == kind {
                    keys.insert(key.name);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "keybindings::deserialize_keys"
    )]
    pub keys: Option<keybindings::KeyOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vim_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_input: Option<RunningInput>,
    /// Runtime-only MCP source records; never configuration or launch trust.
    #[cfg(feature = "mcp")]
    #[serde(skip)]
    pub(crate) mcp_origins: HashMap<String, Vec<crate::extras::mcp::project::ServerOrigin>>,
    /// Validated invocation-only MCP overlays; never persisted or diagnostic data.
    #[cfg(feature = "mcp")]
    #[serde(skip)]
    pub(crate) mcp_cli_overrides: layers::McpOverrides,
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
    /// Global owner ceiling; project layers cannot alter financial policy.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::spend::deserialize_cap"
    )]
    pub max_budget_usd: Option<f64>,
    /// Global owner ceiling on input tokens (cached input included) observed
    /// across the logical session, subagents and scheduled fires included. It
    /// needs no price, so it bounds providers whose prices are unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_session_input_tokens: Option<u64>,
    /// Global owner ceiling on input plus output tokens for the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_session_total_tokens: Option<u64>,
    /// Owner-selected native Anthropic cache TTL. Project layers cannot alter
    /// this billing policy. Omission preserves manual five-minute caching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_ttl: Option<PromptCacheTtl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Provider-specific JSON shallow-merged into every completion request body
    /// as a global default. A matching `quick_models` entry's `extra_body`
    /// overrides this. Note: body params are provider-specific, so a global
    /// value does not follow model switches — bundle per-`quick_models` when in
    /// doubt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Value>,
    /// First-class reasoning controls for the OpenAI family: `effort` and
    /// `summary` map to the Responses API's `reasoning` object (and `effort`
    /// alone to Chat Completions' `reasoning_effort`), `encrypted_content`
    /// governs the `include: ["reasoning.encrypted_content"]` request that
    /// makes persisted reasoning items replayable, and `store` selects
    /// server-side retention. See [`ReasoningConfig`] for the
    /// `store`/`previous_response_id` caveats on gateways that do not persist
    /// response state. A raw `extra_body` key still wins on collision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headless_questions: Option<HeadlessQuestions>,
    /// Opt-in `@agent:<name>` routing for headless `-p` and ACP prompts
    /// (default off: those prompts pass through untouched). Read at startup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headless_agent_mentions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_context_files: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
    /// Number of newest historical tool results kept verbatim in the live
    /// model context. Older results are replaced by compact recovery notices;
    /// the durable session and spill files remain unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent_tool_results: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_agent_turns: Option<usize>,
    /// Fires per `/cron` entry before it is removed (default 24).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron_max_fires: Option<u32>,
    /// Trusted operator command run before a tool-using turn may complete.
    /// Project-local values require the existing sensitive-config approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_command: Option<CompactString>,
    /// Optional formatter argv templates. Project values require the existing
    /// sensitive-config approval; commands retain ordinary shell permission.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub format: BTreeMap<String, String>,
    /// Goal defaults. Benign: these only bound how long the harness keeps
    /// working, never what it is allowed to run.
    #[cfg(feature = "goal")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<Box<types::GoalConfig>>,
    /// Commands that must exit zero before a goal may be reported complete.
    ///
    /// Sensitive by construction: it is a top-level key outside
    /// `BENIGN_PROJECT_CONFIG_KEYS`, so an untrusted project configuration
    /// cannot activate it, exactly as for `verify_command`. It carries the
    /// authority of whoever configured it (`TC-GOAL-CHECK`).
    #[cfg(feature = "goal")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_checks: Option<Vec<CompactString>>,
    /// A `quick_models` entry name to judge goal completion claims.
    ///
    /// Sensitive for the same reason as any other model selection reachable
    /// from project configuration: the judge receives a bounded transcript
    /// tail, so naming its endpoint decides where that text is sent.
    #[cfg(feature = "goal")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_judge_model: Option<CompactString>,
    /// A `quick_models` entry name that titles sessions for `--resume` and
    /// `/sessions` after their first exchange. Unset uses the session model
    /// (the default); `"off"` disables generation and keeps the first message.
    ///
    /// Sensitive like any model selection: the first exchange is sent to the
    /// named entry's endpoint (mini-agent-3wsib).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title_model: Option<CompactString>,
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
    /// Foreground shell tool deadline. Other sandbox command classes retain
    /// their own limits; this does not alter brokered JavaScript spawn limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_timeout_secs: Option<u64>,
    /// Opt-in anonymous release check, only during interactive startup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_check: Option<bool>,
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
    /// Read project `.claude/agents/*.md` as specialist definitions (Claude Code
    /// compatibility). `false` leaves only `.mini-agent/agents`, the user-global
    /// and the built-in layers. Default: `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_project_agents: Option<bool>,
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
        rename = "auto-update-prompts",
        alias = "auto_update_prompts"
    )]
    pub auto_update_prompts: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "auto-update-themes",
        alias = "auto_update_themes"
    )]
    pub auto_update_themes: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_providers: Option<HashMap<String, types::CustomProviderConfig>>,
    /// Embedding backend for the skill library. Absent means the built-in
    /// offline deterministic backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<types::EmbeddingConfig>,
    /// Allow model-authored JavaScript steps to submit bounded learned-skill
    /// proposals. Proposals still require contained verification, held-out
    /// evaluation, and explicit local-owner approval before use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_skill_proposals: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<serde_json::Value>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "permission-regex",
        alias = "permission_regex"
    )]
    pub permission_regex: Option<serde_json::Value>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "permission-allow",
        alias = "permission_allow"
    )]
    pub permission_allow: Option<HashMap<String, Vec<String>>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "permission-allow-prefix",
        alias = "permission_allow_prefix"
    )]
    pub permission_allow_prefix: Option<HashMap<String, Vec<String>>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "permission-ask",
        alias = "permission_ask"
    )]
    pub permission_ask: Option<HashMap<String, Vec<String>>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "permission-deny",
        alias = "permission_deny"
    )]
    pub permission_deny: Option<HashMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restrictive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_all: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "sandbox-backend",
        alias = "sandbox_backend"
    )]
    pub sandbox_backend: Option<String>,
    /// Global-only. When true, startup refuses `--no-sandbox`, `sandbox =
    /// false`, and an unavailable sandbox backend instead of running
    /// unsandboxed.
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "sandbox-required",
        alias = "sandbox_required"
    )]
    pub sandbox_required: Option<bool>,
    /// Global-only. When true, startup refuses `--dangerously-skip-permissions`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "permission-checks-required",
        alias = "permission_checks_required"
    )]
    pub permission_checks_required: Option<bool>,
    /// Extra read-only toolchain roots for the Linux `bwrap` profiles. `None`
    /// selects [`crate::sandbox::default_sandbox_read_roots`]; a configured
    /// list (including an empty one) replaces the default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "sandbox-toolchain-roots",
        alias = "sandbox_toolchain_roots"
    )]
    pub sandbox_toolchain_roots: Option<Vec<std::path::PathBuf>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "sandbox-profile",
        alias = "sandbox_profile"
    )]
    pub sandbox_profile: Option<crate::sandbox::SandboxProfile>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "sandbox-read-roots",
        alias = "sandbox_read_roots"
    )]
    pub sandbox_read_roots: Vec<std::path::PathBuf>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "sandbox-write-roots",
        alias = "sandbox_write_roots"
    )]
    pub sandbox_write_roots: Vec<std::path::PathBuf>,
    /// HOME-relative dot files/folders exposed to contained commands
    /// (read-only, or writable with a `:rw` suffix). Sensitive in project
    /// layers: adding mounts there requires project-config approval.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "sandbox-home-mounts",
        alias = "sandbox_home_mounts"
    )]
    pub sandbox_home_mounts: Vec<String>,
    /// HOME-relative paths hidden in every profile, added to the built-in
    /// private masks. Project layers may only add to this list.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "sandbox-home-masks",
        alias = "sandbox_home_masks"
    )]
    pub sandbox_home_masks: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "windows-appcontainer-read-roots",
        alias = "windows_appcontainer_read_roots"
    )]
    pub windows_appcontainer_read_roots: Vec<std::path::PathBuf>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "windows-appcontainer-write-roots",
        alias = "windows_appcontainer_write_roots"
    )]
    pub windows_appcontainer_write_roots: Vec<std::path::PathBuf>,
    #[cfg(feature = "js")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "js-file-base-dir",
        alias = "js_file_base_dir"
    )]
    pub js_file_base_dir: Option<String>,
    #[cfg(feature = "js")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "js-read-roots",
        alias = "js_read_roots"
    )]
    pub js_read_roots: Option<Vec<String>>,
    #[cfg(feature = "js")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "js-write-roots",
        alias = "js_write_roots"
    )]
    pub js_write_roots: Option<Vec<String>>,
    #[cfg(feature = "js")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "js-read-unrestricted",
        alias = "js_read_unrestricted"
    )]
    pub js_read_unrestricted: Option<bool>,
    #[cfg(feature = "js")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "js-write-unrestricted",
        alias = "js_write_unrestricted"
    )]
    pub js_write_unrestricted: Option<bool>,
    #[cfg(feature = "js")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "js-fetch-origins",
        alias = "js_fetch_origins"
    )]
    pub js_fetch_origins: Option<JsFetchOrigins>,
    #[cfg(feature = "js")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "js-fetch-allow-http",
        alias = "js_fetch_allow_http"
    )]
    pub js_fetch_allow_http: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "web-fetch-origins",
        alias = "web_fetch_origins"
    )]
    pub web_fetch_origins: Option<Vec<String>>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "web-fetch-allow-http",
        alias = "web_fetch_allow_http"
    )]
    pub web_fetch_allow_http: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "web-search-provider",
        alias = "web_search_provider"
    )]
    pub web_search_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "allow-all-mcp-calls")]
    pub allow_all_mcp_calls: Option<bool>,
    /// Bound on one MCP `tools/call` round trip, in seconds. Default: 120.
    #[cfg(feature = "mcp")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_tool_timeout_secs: Option<u64>,
    #[cfg(feature = "mcp")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "enable-exa-mcp",
        alias = "enable_exa_mcp"
    )]
    pub enable_exa_mcp: Option<bool>,
    #[cfg(feature = "mcp")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "enable-context7-mcp",
        alias = "enable_context7_mcp"
    )]
    pub enable_context7_mcp: Option<bool>,
    #[cfg(feature = "mcp")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "enable-grepapp-mcp",
        alias = "enable_grepapp_mcp"
    )]
    pub enable_grepapp_mcp: Option<bool>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "default-permission-mode"
    )]
    pub default_permission_mode: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "permission-modes",
        alias = "permission_modes"
    )]
    pub permission_modes: Option<Vec<String>>,
    /// Explicit-user Shift+Tab cycle, independent of the rule-application modes.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_permission_mode_cycle"
    )]
    pub permission_mode_cycle: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_tool_details: Option<ShowToolDetails>,
    /// How tool results appear in the TUI transcript: `collapsed` (a one-line
    /// summary per call, the default), `preview` (summary plus the first
    /// `show_tool_details` lines, default 3) or `full` (everything). Every
    /// result can be unfolded with Ctrl+T or `/expand`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<ToolOutputMode>,
    /// Context rows in successful edit/write previews (hard maximum 20).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_context_lines: Option<usize>,
    /// Preview row cap (hard maximum 1000); zero disables mutation previews.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_diff_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_display: Option<ReasoningDisplay>,
    /// Configurable status-bar (up to 3 lines). When absent, a built-in
    /// default layout is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statusline: Option<types::StatusLineConfig>,
    /// Left padding (columns) for the chat area. Default: 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_left_margin: Option<u16>,
    /// Capture mouse events in the TUI (wheel scrolling, click-to-place-cursor,
    /// link opening, drag-to-copy). `false` leaves the mouse to the terminal so
    /// its native selection works. Default: true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse_capture: Option<bool>,
    /// Transcript lines one plain mouse-wheel notch scrolls (1..=20).
    /// Shift/Ctrl + wheel always scrolls a page per notch. Ignored when
    /// `mouse_capture = false`. Default: 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse_scroll_lines: Option<u16>,
    /// Report agent activity in the terminal/window title (`mini-agent:
    /// working`, `waiting for approval`, `idle`) so terminal multiplexers and
    /// tab bars can show it; the previous title is restored on exit.
    /// Default: true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<bool>,
    /// Desktop notification escape (`osc9`, `osc777`) emitted when a turn
    /// finishes or a permission prompt waits. Default: off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_notify: Option<TerminalNotify>,
    /// Emit OSC 133 semantic prompt marks: a prompt while the TUI waits for
    /// input, command start while a run is active, command end when it
    /// finishes. Default: false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_prompt_marks: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_prompt: Option<CompactString>,
    #[cfg(feature = "git-worktree")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "wt-auto-merge",
        alias = "wt_auto_merge"
    )]
    pub wt_auto_merge: Option<bool>,
    #[cfg(feature = "git-worktree")]
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "wt-base-dir",
        alias = "wt_base_dir"
    )]
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
    /// mini-agent looks up the quick model and switches provider+model.
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
    #[cfg(feature = "acp")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_accept_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_system: Option<types::EditSystem>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_max_turns: Option<usize>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_context_window: Option<u64>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_max_tokens: Option<u64>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_max_report_bytes: Option<usize>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_report_repair_max_tokens: Option<u64>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_report_repair_max_cost_units: Option<u64>,
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
    pub task_cost_weighting: Option<TaskCostWeighting>,
    /// `hint` (default) or `force`: whether an `@agent:<name>` mention turn
    /// forces its first model request to call the `task` tool. Startup key.
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_mention_tool_choice: Option<AgentMentionToolChoice>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_cost_weights: Option<TaskCostWeights>,
    #[cfg(feature = "subagents")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_cost_overrides: Option<HashMap<String, TaskCostOverride>>,
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
    pub subagents: Option<types::SubagentPolicy>,
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
    /// Warnings raised while loading (unknown keys, clamped values, ...),
    /// kept so `--print-config` can repeat what startup printed. Never read
    /// from or written to a config file.
    #[serde(skip)]
    pub load_warnings: Vec<String>,
    /// Load-time metadata, never persisted or sent to a model.
    #[serde(skip)]
    pub provenance: ConfigProvenance,
    /// Interactive startup presentation only; never persisted or sent to a model.
    #[serde(skip)]
    pub update_notice: Option<String>,
}

/// The result of [`Config::resolve_prompt_route`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum PromptRoute<'a> {
    /// The prompt has no (or an empty) `[prompt_to_model]` entry.
    Unmapped,
    /// The prompt routes to quick model `alias`.
    Mapped {
        alias: &'a str,
        quick_model: &'a types::QuickModelConfig,
    },
    /// The prompt routes to `alias`, which names no `[quick_models]` entry.
    Dangling {
        // Only ACP persona refusals report the dangling alias.
        #[cfg_attr(not(feature = "acp"), allow(dead_code))]
        alias: &'a str,
    },
}

impl Config {
    pub(crate) fn mutation_diff_options(&self) -> crate::agent::tools::mutation_diff::DiffOptions {
        crate::agent::tools::mutation_diff::DiffOptions {
            context: self.diff_context_lines.unwrap_or(3).min(20),
            max_lines: self
                .max_diff_lines
                .unwrap_or(200)
                .min(crate::agent::tools::mutation_diff::MAX_DIFF_LINES),
        }
    }

    /// Borrowed custom-provider table; absent reads as a shared empty map.
    pub fn custom_providers_map(&self) -> &HashMap<String, types::CustomProviderConfig> {
        static EMPTY: std::sync::LazyLock<HashMap<String, types::CustomProviderConfig>> =
            std::sync::LazyLock::new(HashMap::new);
        self.custom_providers.as_ref().unwrap_or(&EMPTY)
    }

    /// Whether requests for `provider` go through the Anthropic-native API
    /// route. This is the route that enables prompt caching and reports
    /// `input_tokens` *excluding* cached/cache-creation tokens, so it is the
    /// route whose context accounting must always add the cache fields back in
    /// (see [`Session::real_input_tokens`](crate::session::Session::real_input_tokens)).
    ///
    /// Keyed on the resolved provider *kind*, not the user-facing name: a
    /// custom provider registered under any name but with
    /// `provider_type = "anthropic"` or native Bedrock Claude Messages still
    /// hits the native route, while
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
            Some(
                crate::auth::ProviderKind::Anthropic
                    | crate::auth::ProviderKind::Bedrock
                    | crate::auth::ProviderKind::Vertex
            )
        )
    }

    /// Whether `provider` reports reasoning/thinking tokens *in addition to*
    /// its output-token count rather than as a subset of it.
    ///
    /// Gemini is the exception: `thoughtsTokenCount` is reported alongside
    /// `candidatesTokenCount`, so reasoning has to be added to reconstruct
    /// what the turn actually cost. OpenAI (Responses and Chat Completions),
    /// Anthropic and the OpenAI-compatible gateways all fold reasoning into
    /// `output_tokens` — adding it there charges the same tokens twice.
    pub fn reasoning_tokens_are_exclusive_of_output(&self, provider: &str) -> bool {
        let kind_name = self
            .custom_providers
            .as_ref()
            .and_then(|m| m.get(provider))
            .map(|c| c.provider_type.as_str())
            .unwrap_or(provider);
        matches!(
            crate::auth::ProviderKind::from_name(kind_name),
            Some(crate::auth::ProviderKind::Gemini)
        )
    }

    /// `usage` in the shape every other provider reports: reasoning tokens
    /// that `provider` counts separately are folded into `output_tokens`.
    ///
    /// Gemini's thinking tokens are billed as output but reported beside it,
    /// so without this the session and `--output json` never charged them, and
    /// `total_tokens - output_tokens` counted them as prompt, inflating the
    /// context estimate (mini-agent-izx3e). Apply it exactly once per usage
    /// report, before charging or pricing it.
    pub fn normalize_usage(
        &self,
        provider: &str,
        mut usage: crate::event::UsageDelta,
    ) -> crate::event::UsageDelta {
        if self.reasoning_tokens_are_exclusive_of_output(provider) {
            usage.output_tokens = usage.output_tokens.saturating_add(usage.reasoning_tokens);
        }
        usage
    }

    pub(crate) fn has_session_token_ceiling(&self) -> bool {
        self.max_session_input_tokens.is_some() || self.max_session_total_tokens.is_some()
    }

    /// One-line notice for a model whose context window nothing knows, so the
    /// 128k fallback is in effect.
    pub fn context_window_fallback_notice(provider: &str, model: &str) -> String {
        format!(
            "notice: no context window is known for {provider}/{model}; assuming 128000 tokens. \
             Set `context_window` on its quick model so compaction and the context gauge match the model."
        )
    }

    pub fn resolve_context_window(
        &self,
        provider: &str,
        model_id: &str,
        qm: &HashMap<String, types::QuickModelConfig>,
    ) -> u64 {
        self.known_context_window(provider, model_id, qm)
            .unwrap_or(128_000)
    }

    /// [`Self::resolve_context_window`] without the 128k fallback: the pinned
    /// `context_window`, then a quick model's, then the catalog's, or `None`
    /// when none of them knows the model.
    pub fn known_context_window(
        &self,
        provider: &str,
        model_id: &str,
        qm: &HashMap<String, types::QuickModelConfig>,
    ) -> Option<u64> {
        if let Some(cw) = self.context_window {
            return Some(cw);
        }
        for qmc in qm.values() {
            if qmc.model.as_str() == model_id
                && let Some(cw) = qmc.context_window
            {
                return Some(cw);
            }
        }
        Self::catalog_context_window(provider, model_id)
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

    /// Whether headless `-p` and ACP prompts resolve `@agent:<name>`
    /// mentions. Off by default so scripts piping arbitrary text keep their
    /// behaviour; inert in builds without the `subagents` feature.
    #[cfg_attr(not(feature = "subagents"), allow(dead_code))]
    pub fn resolve_headless_agent_mentions(&self) -> bool {
        self.headless_agent_mentions.unwrap_or(false)
    }

    pub fn resolve_keep_recent_tool_results(&self) -> usize {
        self.keep_recent_tool_results
            .unwrap_or(crate::session::DEFAULT_KEEP_RECENT_TOOL_RESULTS)
    }

    /// Cumulative per-turn input+output token cap. `None` (default) means no
    /// cap; see the field docs. Never derived from `max_tokens`, which caps a
    /// single response, not a turn.
    /// Fires one `/cron` entry may start before it is removed; at least 1.
    pub fn resolve_cron_max_fires(&self) -> u32 {
        self.cron_max_fires
            .unwrap_or(crate::ui::cron::DEFAULT_CRON_MAX_FIRES)
            .max(1)
    }

    pub fn resolve_turn_token_budget(&self) -> Option<u64> {
        self.turn_token_budget
    }

    pub fn resolve_verify_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.verify_timeout_secs.unwrap_or(300).clamp(1, 3_600))
    }

    /// Which model judges completion claims.
    ///
    /// The judge is whatever the user names: another family, another size, or
    /// the session's own model. `Auto` falls back to the session model so a
    /// single-model install still gets a second opinion from a clean context.
    /// Whether the user named a judge at all, rather than taking the default.
    ///
    /// A `--loop` preset uses this to keep its historical shape: a loop has
    /// never called a second model, so it only does so when the installation
    /// asked for one by name. Gated with its only consumer; `loop` implies
    /// `goal`, so a goal-only build has no preset to ask.
    #[cfg(feature = "loop")]
    pub fn goal_judge_configured(&self) -> bool {
        self.goal_judge_model.is_some() || self.goal.as_ref().is_some_and(|g| g.judge.is_some())
    }

    #[cfg(feature = "goal")]
    pub fn resolve_goal_judge(&self) -> crate::extras::goal::JudgePolicy {
        use crate::extras::goal::JudgePolicy;
        match self.goal.as_ref().and_then(|g| g.judge.as_deref()) {
            None | Some("auto") => JudgePolicy::Auto,
            Some("off") => JudgePolicy::Off,
            Some("session") => JudgePolicy::Session,
            Some(name) => JudgePolicy::QuickModel(name.to_string()),
        }
    }

    pub fn resolve_verify_max_attempts(&self) -> u32 {
        self.verify_max_attempts.unwrap_or(3).clamp(1, 8)
    }

    pub fn resolve_chat_left_margin(&self) -> u16 {
        self.chat_left_margin.unwrap_or(0)
    }

    pub fn resolve_mouse_capture(&self) -> bool {
        self.mouse_capture.unwrap_or(true)
    }

    pub fn resolve_mouse_scroll_lines(&self) -> usize {
        usize::from(self.mouse_scroll_lines.unwrap_or(3).clamp(1, 20))
    }

    pub fn resolve_terminal_title(&self) -> bool {
        self.terminal_title.unwrap_or(true)
    }

    pub fn resolve_terminal_notify(&self) -> TerminalNotify {
        self.terminal_notify.unwrap_or_default()
    }

    pub fn resolve_terminal_prompt_marks(&self) -> bool {
        self.terminal_prompt_marks.unwrap_or(false)
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

    pub fn resolve_shell_timeout_secs(&self) -> u64 {
        self.shell_timeout_secs
            .unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS)
            .clamp(1, MAX_SHELL_TIMEOUT_SECS)
    }

    /// LSP policy only; executable discovery belongs to owned agent preparation.
    /// An omitted table uses automatic discovery and explicit false stays off.
    #[cfg(feature = "lsp")]
    pub fn resolve_lsp(&self) -> Option<std::borrow::Cow<'_, types::LspConfig>> {
        match self.lsp.as_ref() {
            Some(config) if config.enabled == types::LspActivation::Off => None,
            Some(config) => Some(std::borrow::Cow::Borrowed(config)),
            None => Some(std::borrow::Cow::Owned(types::LspConfig::default())),
        }
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

    /// Independent child input capacity; the main session's context pin is not
    /// evidence about another model. Explicit matching quick-model metadata
    /// and the selected provider's catalogue determine the automatic capacity,
    /// capped at 1 Mi tokens; unknown models use 128 Ki tokens. An explicit cap
    /// can replace that default, but never exceed a known model capacity.
    #[cfg(feature = "subagents")]
    pub fn resolve_subagent_context_window(
        &self,
        provider: &str,
        model: &str,
        alias: Option<&str>,
    ) -> u64 {
        let quick = quick_models_map(self);
        let selected = alias
            .and_then(|alias| quick.get(alias))
            .filter(|entry| entry.provider == provider && entry.model == model)
            .and_then(|entry| entry.context_window)
            .or_else(|| {
                quick
                    .values()
                    .filter(|entry| entry.provider == provider && entry.model == model)
                    .filter_map(|entry| entry.context_window)
                    .min()
            });
        let kind = self
            .custom_providers
            .as_ref()
            .and_then(|providers| providers.get(provider))
            .map(|entry| entry.provider_type.as_str())
            .unwrap_or(provider);
        let catalog_provider = if self.is_anthropic_native(provider) {
            "anthropic"
        } else {
            kind
        };
        let catalog = Self::catalog_context_window(catalog_provider, model);
        let known = match (selected, catalog) {
            (Some(selected), Some(catalog)) => Some(selected.min(catalog)),
            (selected, catalog) => selected.or(catalog),
        };
        let requested = self
            .subagent_context_window
            .unwrap_or_else(|| known.map_or(131_072, |capacity| capacity.min(1_048_576)));
        known.map_or(requested, |capacity| requested.min(capacity))
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_subagent_max_tokens(&self) -> u64 {
        self.subagent_max_tokens.unwrap_or(32_000)
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_max_report_bytes(&self) -> usize {
        self.task_max_report_bytes
            .unwrap_or(32_768)
            .min(self.resolve_task_max_output_bytes())
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_report_repair_max_tokens(&self) -> u64 {
        self.task_report_repair_max_tokens
            .unwrap_or(8192)
            .min(self.resolve_subagent_max_tokens())
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_report_repair_max_cost_units(&self) -> u64 {
        self.task_report_repair_max_cost_units.unwrap_or(65_536)
    }

    #[cfg(feature = "subagents")]
    pub(crate) fn validate_subagent_settings(&self) -> Result<(), String> {
        if let Some(weights) = self.task_cost_weights {
            weights
                .validate()
                .map_err(|field| format!("Invalid task_cost_weights.{field} (value omitted)"))?;
        }
        if let Some(overrides) = &self.task_cost_overrides {
            for (alias, policy) in overrides {
                if alias.trim().is_empty() {
                    return Err("Invalid task_cost_overrides alias (value omitted)".into());
                }
                if let Some(weights) = policy.weights {
                    weights.validate().map_err(|field| {
                        format!("Invalid task_cost_overrides.weights.{field} (value omitted)")
                    })?;
                }
            }
        }
        for (name, invalid) in [
            (
                "subagent_context_window",
                self.subagent_context_window
                    .is_some_and(|value| value < 4096),
            ),
            ("subagent_max_tokens", self.subagent_max_tokens == Some(0)),
            ("task_max_turns", self.task_max_turns == Some(0)),
            ("task_max_prompts", self.task_max_prompts == Some(0)),
            ("task_max_concurrency", self.task_max_concurrency == Some(0)),
            (
                "task_max_output_bytes",
                self.task_max_output_bytes.is_some_and(|value| value < 256),
            ),
            (
                "task_max_report_bytes",
                self.task_max_report_bytes.is_some_and(|value| value < 256),
            ),
            ("task_max_cost_units", self.task_max_cost_units == Some(0)),
            (
                "task_report_repair_max_tokens",
                self.task_report_repair_max_tokens == Some(0),
            ),
            (
                "task_report_repair_max_cost_units",
                self.task_report_repair_max_cost_units == Some(0),
            ),
            (
                "task_timeout_secs",
                self.task_timeout_secs
                    .is_some_and(|value| value == 0 || value > 86_400),
            ),
        ] {
            if invalid {
                return Err(format!("Invalid {name} (value omitted)"));
            }
        }
        Ok(())
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
        self.task_max_output_bytes.unwrap_or(128 * 1024)
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_cost_weighting(&self) -> TaskCostWeighting {
        self.task_cost_weighting.unwrap_or_default()
    }

    /// Mention-turn `tool_choice` policy; `hint` unless explicitly `force`.
    #[cfg(feature = "subagents")]
    pub fn resolve_agent_mention_tool_choice(&self) -> AgentMentionToolChoice {
        self.agent_mention_tool_choice.unwrap_or_default()
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_cost_weights(&self) -> TaskCostWeights {
        self.task_cost_weights.unwrap_or_default()
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_max_cost_units(&self) -> u64 {
        self.task_max_cost_units.unwrap_or(4_000_000)
    }

    #[cfg(feature = "subagents")]
    pub fn resolve_task_timeout_secs(&self) -> u64 {
        self.task_timeout_secs.unwrap_or(300)
    }

    pub fn resolve_always_show_welcome(&self) -> bool {
        self.always_show_welcome.unwrap_or(false)
    }

    pub fn resolve_show_reasoning(&self) -> bool {
        self.resolve_reasoning_display() != ReasoningDisplay::Off
    }

    pub fn resolve_reasoning_display(&self) -> ReasoningDisplay {
        self.reasoning_display.unwrap_or(match self.show_reasoning {
            Some(false) => ReasoningDisplay::Off,
            Some(true) => ReasoningDisplay::Full,
            None => ReasoningDisplay::Collapsed,
        })
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

    /// The prompt the main loop starts with when `--load-prompt` names none:
    /// `default_prompt`, or the embedded `code` prompt.
    pub(crate) fn resolve_default_prompt(&self) -> &str {
        self.default_prompt.as_deref().unwrap_or("code")
    }

    /// Where `[prompt_to_model]` routes prompt `prompt_name`: unmapped, to an
    /// existing `[quick_models]` entry, or to an alias that names no quick
    /// model. Startup, the TUI prompt switch and ACP sessions all resolve the
    /// route here, so each surface agrees on which model a prompt selects.
    pub(crate) fn resolve_prompt_route(&self, prompt_name: &str) -> PromptRoute<'_> {
        let Some(alias) = self.resolve_prompt_model(prompt_name) else {
            return PromptRoute::Unmapped;
        };
        match self.quick_models.as_ref().and_then(|qm| qm.get(alias)) {
            Some(quick_model) => PromptRoute::Mapped { alias, quick_model },
            None => PromptRoute::Dangling { alias },
        }
    }

    pub fn resolve_claude_project_agents(&self) -> bool {
        self.claude_project_agents.unwrap_or(true)
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

    /// Explain authored shell allow metacharacters once during config loading.
    /// Diagnostics identify the field/entry without echoing script secrets.
    pub(crate) fn shell_exact_allow_warnings(&self) -> Vec<String> {
        let mut entries = Vec::new();
        for (field, value) in [
            ("permission", &self.permission),
            ("permission-regex", &self.permission_regex),
        ] {
            if let Some(map) = value.as_ref().and_then(serde_json::Value::as_object) {
                for tool in ["bash", "shell"] {
                    if let Some(rule) = map.get(tool) {
                        if rule.as_str() == Some("allow") {
                            entries.push(format!("{field}.{tool}"));
                        }
                        if let Some(patterns) = rule.as_object() {
                            for (index, (pattern, action)) in patterns.iter().enumerate() {
                                if action.as_str() == Some("allow")
                                    && pattern.contains(['*', '?', '['])
                                {
                                    entries.push(format!("{field}.{tool} entry {}", index + 1));
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(map) = &self.permission_allow {
            for tool in ["bash", "shell"] {
                if let Some(patterns) = map.get(tool) {
                    for (index, pattern) in patterns.iter().enumerate() {
                        if pattern.contains(['*', '?', '[']) {
                            entries.push(format!("permission-allow.{tool} entry {}", index + 1));
                        }
                    }
                }
            }
        }
        entries.into_iter().map(|entry| format!("{entry}: shell/bash allow rules match the exact complete script; metacharacters do not grant a glob scope. Use an explicit script or permission-allow-prefix for a simple argv prefix.")).collect()
    }

    pub fn build_permission_config(&self) -> anyhow::Result<PermissionConfigs> {
        crate::permission::permission_mode_cycle(self.permission_mode_cycle.as_deref())
            .map_err(anyhow::Error::msg)?;
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
                    if matches!(tool.as_str(), "bash" | "shell")
                        && configured.as_str() == Some("allow")
                    {
                        anyhow::bail!(
                            "invalid `{field}` configuration at `{tool}`: shell allow rules must name the exact complete script"
                        );
                    }
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

        if let Some(entries) = &self.permission_allow_prefix {
            for (tool, scripts) in entries {
                if !matches!(tool.as_str(), "bash" | "shell") {
                    anyhow::bail!("permission-allow-prefix supports only bash/shell");
                }
                for script in scripts {
                    crate::permission::shell_prefix::ShellPrefix::parse(script)?;
                }
            }
            perm_configs.glob.allow_prefix_entries = Some(entries.clone());
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

fn deserialize_permission_mode_cycle<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let configured = Option::<Vec<String>>::deserialize(deserializer)?;
    crate::permission::permission_mode_cycle(configured.as_deref())
        .map_err(serde::de::Error::custom)?;
    Ok(configured)
}

/// `js-fetch-origins`: the string `"all"` (no origin narrowing), the string
/// `"none"` (deny every fetch), or an array of exact origins. The permission
/// check on `js/fetch` applies in every form.
#[cfg(feature = "js")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsFetchOrigins {
    All,
    None,
    List(Vec<String>),
}

#[cfg(feature = "js")]
impl JsFetchOrigins {
    const EXPECTED: &'static str =
        "js-fetch-origins must be \"all\", \"none\", or an array of exact origins";

    /// The slice form `FetchPolicy::from_settings` takes: `None` is
    /// unrestricted, an empty list denies everything.
    pub fn as_policy_origins(&self) -> Option<&[String]> {
        match self {
            Self::All => None,
            Self::None => Some(&[]),
            Self::List(origins) => Some(origins),
        }
    }
}

#[cfg(feature = "js")]
impl Serialize for JsFetchOrigins {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::All => serializer.serialize_str("all"),
            Self::None => serializer.serialize_str("none"),
            Self::List(origins) => origins.serialize(serializer),
        }
    }
}

#[cfg(feature = "js")]
impl<'de> Deserialize<'de> for JsFetchOrigins {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Word(String),
            List(Vec<String>),
        }
        match Raw::deserialize(deserializer)
            .map_err(|_| serde::de::Error::custom(Self::EXPECTED))?
        {
            Raw::Word(word) if word == "all" => Ok(Self::All),
            Raw::Word(word) if word == "none" => Ok(Self::None),
            Raw::Word(_) => Err(serde::de::Error::custom(Self::EXPECTED)),
            Raw::List(origins) => Ok(Self::List(origins)),
        }
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
    cfg.resolve_temperature(cli, model_id, quick_models_map(cfg))
}

/// Convenience: resolves extra body params (quick model, global config).
pub fn resolve_extra_body(cfg: &Config, model_id: &str) -> Option<serde_json::Value> {
    cfg.resolve_extra_body(model_id, quick_models_map(cfg))
}

/// The `tool_output` config value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolOutputMode {
    Collapsed,
    Preview,
    Full,
}

/// How the TUI renders a tool result, after combining `tool_output` with the
/// legacy `show_tool_details` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedToolOutput {
    /// One summary line; the output is shown only when unfolded.
    Collapsed,
    /// Summary plus the first `n` lines while folded.
    Preview(usize),
    /// Summary plus the whole output, unfolded from the start.
    Full,
}

/// Preview length used when `tool_output = "preview"` names no line count.
pub const DEFAULT_TOOL_PREVIEW_LINES: usize = 3;

impl Config {
    /// Resolve the tool-result display mode. `tool_output` wins; without it an
    /// explicit legacy `show_tool_details` keeps its old meaning (an integer
    /// previews that many lines, `true` shows everything, `false` collapses),
    /// and with neither results are collapsed.
    pub fn resolve_tool_output(&self) -> ResolvedToolOutput {
        let preview_lines = match self.show_tool_details {
            Some(ShowToolDetails::Lines(n)) if n > 0 => n,
            _ => DEFAULT_TOOL_PREVIEW_LINES,
        };
        match self.tool_output {
            Some(ToolOutputMode::Collapsed) => ResolvedToolOutput::Collapsed,
            Some(ToolOutputMode::Preview) => ResolvedToolOutput::Preview(preview_lines),
            Some(ToolOutputMode::Full) => ResolvedToolOutput::Full,
            None => match self.show_tool_details.map(|s| s.resolve()) {
                None | Some(ResolvedShowToolDetails::Off) => ResolvedToolOutput::Collapsed,
                Some(ResolvedShowToolDetails::Limited(0)) => ResolvedToolOutput::Collapsed,
                Some(ResolvedShowToolDetails::Limited(n)) => ResolvedToolOutput::Preview(n),
                Some(ResolvedShowToolDetails::Unlimited) => ResolvedToolOutput::Full,
            },
        }
    }
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

impl Config {
    pub(crate) fn native_cache_split_required(&self, model: &str) -> bool {
        let extras = resolve_extra_body(self, model);
        let raw_override = extras.as_ref().is_some_and(|extras| {
            let marked = |block: &serde_json::Value| {
                block
                    .get("cache_control")
                    .is_some_and(|value| !value.is_null())
            };
            marked(extras)
                || extras.get("system").is_some()
                || extras.get("messages").is_some()
                || extras
                    .get("tools")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|tools| tools.iter().any(marked))
        });
        self.cache_ttl == Some(PromptCacheTtl::OneHour) || raw_override
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn shell_deadline_configuration_has_a_finite_hard_ceiling() {
        assert_eq!(Config::default().resolve_shell_timeout_secs(), 120);
        for (requested, expected) in [(0, 1), (1, 1), (121, 121), (600, 600), (u64::MAX, 600)] {
            let cfg = Config {
                shell_timeout_secs: Some(requested),
                ..Default::default()
            };
            assert_eq!(cfg.resolve_shell_timeout_secs(), expected);
        }
        assert_eq!(
            crate::sandbox::DEFAULT_COMMAND_LIMITS.timeout.as_secs(),
            30,
            "raising the shell tool deadline must not change other command classes"
        );
    }

    #[test]
    fn shell_deadline_is_an_owned_roundtrip_config_key() {
        let path = std::path::Path::new("config.toml");
        let mut cfg =
            parse_config_content(path, "shell_timeout_secs = 321\nfuture_key = 'kept'\n").unwrap();
        assert_eq!(cfg.resolve_shell_timeout_secs(), 321);
        assert!(!cfg.preserved.keys().any(|key| key == "shell_timeout_secs"));
        cfg.shell_timeout_secs = Some(9999);
        let content = serialize_config_content(path, &cfg).unwrap();
        let reloaded = parse_config_content(path, &content).unwrap();
        assert_eq!(reloaded.shell_timeout_secs, Some(9999));
        assert_eq!(
            reloaded.resolve_shell_timeout_secs(),
            MAX_SHELL_TIMEOUT_SECS
        );
        assert_eq!(
            reloaded
                .preserved
                .0
                .get("future_key")
                .and_then(toml::Value::as_str),
            Some("kept")
        );
        cfg.shell_timeout_secs = None;
        let removed = serialize_config_content(path, &cfg).unwrap();
        assert!(!removed.contains("shell_timeout_secs"));
    }

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
    fn keep_recent_tool_results_has_a_configurable_default() {
        assert_eq!(
            Config::default().resolve_keep_recent_tool_results(),
            crate::session::DEFAULT_KEEP_RECENT_TOOL_RESULTS
        );
        let explicit = Config {
            keep_recent_tool_results: Some(3),
            ..Default::default()
        };
        assert_eq!(explicit.resolve_keep_recent_tool_results(), 3);
    }

    #[test]
    fn headless_agent_mentions_is_opt_in() {
        assert!(!Config::default().resolve_headless_agent_mentions());
        let parsed: Config = toml::from_str("headless_agent_mentions = true").unwrap();
        assert!(parsed.resolve_headless_agent_mentions());
        let off: Config = toml::from_str("headless_agent_mentions = false").unwrap();
        assert!(!off.resolve_headless_agent_mentions());
    }

    #[test]
    fn cron_max_fires_defaults_to_24_and_is_at_least_one() {
        assert_eq!(Config::default().resolve_cron_max_fires(), 24);
        let explicit = Config {
            cron_max_fires: Some(3),
            ..Config::default()
        };
        assert_eq!(explicit.resolve_cron_max_fires(), 3);
        let zero = Config {
            cron_max_fires: Some(0),
            ..Config::default()
        };
        assert_eq!(zero.resolve_cron_max_fires(), 1);
        let parsed: Config = toml::from_str("cron_max_fires = 5").unwrap();
        assert_eq!(parsed.resolve_cron_max_fires(), 5);
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
    fn resolve_prompt_route_distinguishes_mapped_unmapped_and_dangling() {
        let cfg: Config = toml::from_str(
            r#"
[quick_models.fast]
provider = "acme"
model = "acme-fast"

[prompt_to_model]
plan = "fast"
review = "missing"
brainstorm = ""
"#,
        )
        .unwrap();
        match cfg.resolve_prompt_route("plan") {
            PromptRoute::Mapped { alias, quick_model } => {
                assert_eq!(alias, "fast");
                assert_eq!(quick_model.provider, "acme");
                assert_eq!(quick_model.model, "acme-fast");
            }
            other => panic!("plan must be mapped: {other:?}"),
        }
        assert!(matches!(
            cfg.resolve_prompt_route("review"),
            PromptRoute::Dangling { alias: "missing" }
        ));
        assert!(matches!(
            cfg.resolve_prompt_route("brainstorm"),
            PromptRoute::Unmapped
        ));
        assert!(matches!(
            cfg.resolve_prompt_route("code"),
            PromptRoute::Unmapped
        ));
        assert_eq!(cfg.resolve_default_prompt(), "code");
    }

    #[test]
    fn resolve_prompt_model_returns_none_for_empty_map() {
        let cfg = make_config(HashMap::new());
        assert_eq!(cfg.resolve_prompt_model("plan"), None);
    }

    #[test]
    fn toml_terminal_title_defaults_on_and_can_be_disabled() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.resolve_terminal_title());
        let cfg: Config = toml::from_str("terminal_title = false\n").unwrap();
        assert_eq!(cfg.terminal_title, Some(false));
        assert!(!cfg.resolve_terminal_title());
    }

    /// mini-agent-qfrps: notifications and prompt marks are opt-in.
    #[test]
    fn toml_terminal_notify_and_prompt_marks_default_off() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.resolve_terminal_notify(), TerminalNotify::Off);
        assert!(!cfg.resolve_terminal_prompt_marks());
        let cfg: Config =
            toml::from_str("terminal_notify = \"osc777\"\nterminal_prompt_marks = true\n").unwrap();
        assert_eq!(cfg.resolve_terminal_notify(), TerminalNotify::Osc777);
        assert!(cfg.resolve_terminal_prompt_marks());
        let cfg: Config = toml::from_str("terminal_notify = \"osc9\"\n").unwrap();
        assert_eq!(cfg.resolve_terminal_notify(), TerminalNotify::Osc9);
        assert!(toml::from_str::<Config>("terminal_notify = \"bell\"\n").is_err());
    }

    #[test]
    fn toml_mouse_scroll_lines_defaults_to_three_and_is_clamped() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.resolve_mouse_scroll_lines(), 3);
        for (text, expected) in [("5", 5), ("0", 1), ("500", 20)] {
            let cfg: Config = toml::from_str(&format!("mouse_scroll_lines = {text}\n")).unwrap();
            assert_eq!(cfg.resolve_mouse_scroll_lines(), expected, "{text}");
        }
    }

    #[test]
    fn toml_mouse_capture_defaults_on_and_can_be_disabled() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.resolve_mouse_capture());
        let cfg: Config = toml::from_str("mouse_capture = false\n").unwrap();
        assert_eq!(cfg.mouse_capture, Some(false));
        assert!(!cfg.resolve_mouse_capture());
        let cfg: Config = toml::from_str("mouse_capture = true\n").unwrap();
        assert!(cfg.resolve_mouse_capture());
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
            Some(JsFetchOrigins::List(vec![
                "https://example.com".to_string(),
                "http://public.example:8080".to_string()
            ]))
        );
        assert_eq!(cfg.js_fetch_allow_http, Some(true));
    }

    #[cfg(feature = "js")]
    #[test]
    fn js_fetch_origins_accepts_all_none_and_lists() {
        let parse = |value: &str| {
            toml::from_str::<Config>(&format!("js-fetch-origins = {value}"))
                .map(|cfg| cfg.js_fetch_origins)
        };
        assert_eq!(parse("\"all\"").unwrap(), Some(JsFetchOrigins::All));
        assert_eq!(parse("\"none\"").unwrap(), Some(JsFetchOrigins::None));
        assert_eq!(
            parse("[\"https://docs.rs\"]").unwrap(),
            Some(JsFetchOrigins::List(vec!["https://docs.rs".into()]))
        );
        assert_eq!(parse("[]").unwrap(), Some(JsFetchOrigins::List(vec![])));
        let snake: Config = toml::from_str("js_fetch_origins = \"none\"").unwrap();
        assert_eq!(snake.js_fetch_origins, Some(JsFetchOrigins::None));
        for bad in ["\"any\"", "\"ALL\"", "\"\"", "true", "3"] {
            let error = parse(bad).unwrap_err().to_string();
            assert!(
                error.contains("\"all\"") && error.contains("\"none\"") && error.contains("array"),
                "{bad}: {error}"
            );
        }
        let round_trip = |origins: JsFetchOrigins| {
            let cfg = Config {
                js_fetch_origins: Some(origins),
                ..Config::default()
            };
            toml::from_str::<Config>(&toml::to_string(&cfg).unwrap())
                .unwrap()
                .js_fetch_origins
        };
        assert_eq!(round_trip(JsFetchOrigins::All), Some(JsFetchOrigins::All));
        assert_eq!(round_trip(JsFetchOrigins::None), Some(JsFetchOrigins::None));
    }

    #[cfg(all(feature = "js", feature = "sandbox"))]
    #[test]
    fn js_fetch_origins_forms_drive_the_fetch_policy() {
        use crate::agent::tools::web_http::FetchPolicy;
        let url = "https://example.org/page";
        let policy = |origins: JsFetchOrigins| {
            FetchPolicy::from_settings(origins.as_policy_origins(), false)
        };
        assert!(policy(JsFetchOrigins::All).authorize(url).is_ok());
        assert!(policy(JsFetchOrigins::None).authorize(url).is_err());
        let docs = JsFetchOrigins::List(vec!["https://docs.rs".into()]);
        assert!(policy(docs.clone()).authorize(url).is_err());
        assert!(policy(docs).authorize("https://docs.rs/x").is_ok());
        assert!(policy(JsFetchOrigins::List(vec![])).authorize(url).is_err());
    }
}

#[cfg(all(test, feature = "subagents"))]
mod subagent_settings127_tests {
    use super::*;

    #[test]
    fn subagent130_only_explicit_parent_response_limits_constrain_children() {
        let mut cfg = load::rich_default_config();
        assert_eq!(cfg.max_tokens, None);
        assert_eq!(crate::cli::Cli::default().resolve_max_tokens(&cfg), 16_384);
        assert_eq!(cfg.resolve_subagent_max_tokens(), 32_000);
        // First startup persists these defaults. Loading them again must not
        // convert the implicit main response fallback into a child ceiling.
        let serialized =
            load::serialize_config_content(std::path::Path::new("config.toml"), &cfg).unwrap();
        cfg = load::parse_config_content(std::path::Path::new("config.toml"), &serialized).unwrap();
        assert_eq!(cfg.max_tokens, None);
        assert_eq!(crate::cli::Cli::default().resolve_max_tokens(&cfg), 16_384);
        assert_eq!(cfg.resolve_subagent_max_tokens(), 32_000);
        cfg = load::merge_config_value_layer(
            &cfg,
            toml::from_str("max_tokens = 16384").unwrap(),
            ConfigLayer::Cli,
        )
        .unwrap();
        assert_eq!(cfg.max_tokens, Some(16_384));
        let raw = Config {
            max_tokens: Some(16_384),
            ..Default::default()
        };
        assert_eq!(raw.max_tokens, Some(16_384));
    }

    #[test]
    fn subagent130_automatic_capacity_uses_child_metadata_and_safe_unknown_default() {
        for mut cfg in [Config::default(), load::rich_default_config()] {
            // The default layer must leave automatic sizing unpinned.
            assert_eq!(cfg.subagent_context_window, None);
            cfg.context_window = Some(1_000_000);
            cfg.quick_models = serde_json::from_value(serde_json::json!({
                "small": {"provider":"custom", "model":"small", "context_window":200000},
                "large": {"provider":"custom", "model":"large", "context_window":1048576},
                "larger": {"provider":"custom", "model":"larger", "context_window":2097152}
            }))
            .unwrap();
            for (model, expected) in [
                ("small", 200_000),
                ("large", 1_048_576),
                ("larger", 1_048_576),
                ("unknown", 131_072),
            ] {
                assert_eq!(
                    cfg.resolve_subagent_context_window("custom", model, Some(model)),
                    expected
                );
            }
            // Matching a model name under another provider proves no capacity.
            assert_eq!(
                cfg.resolve_subagent_context_window("other", "small", Some("small")),
                131_072
            );
            cfg.subagent_context_window = Some(524_288);
            assert_eq!(
                cfg.resolve_subagent_context_window("custom", "small", Some("small")),
                200_000
            );
            assert_eq!(
                cfg.resolve_subagent_context_window("custom", "large", Some("large")),
                524_288
            );
            assert_eq!(
                cfg.resolve_subagent_context_window("custom", "unknown", None),
                524_288
            );
        }
    }

    #[test]
    fn subagent127_defaults_and_roundtrip_keep_context_response_and_reports_distinct() {
        let cfg = Config::default();
        assert_eq!(
            cfg.resolve_subagent_context_window("custom", "unknown", None),
            131_072
        );
        assert_eq!(cfg.resolve_subagent_max_tokens(), 32_000);
        assert_eq!(cfg.resolve_task_max_report_bytes(), 32_768);
        assert_eq!(cfg.resolve_task_max_output_bytes(), 131_072);
        assert_eq!(cfg.resolve_task_max_cost_units(), 4_000_000);
        assert_eq!(cfg.resolve_task_report_repair_max_tokens(), 8192);
        assert_eq!(cfg.resolve_task_report_repair_max_cost_units(), 65_536);
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "context_window": 1_000_000, "subagent_context_window": 1_000_000,
            "subagent_max_tokens": 32768, "task_max_report_bytes": 524288,
            "task_max_output_bytes": 1048576, "task_report_repair_max_tokens": 8192,
            "task_report_repair_max_cost_units": 100000,
            "quick_models": {"child":{"provider":"custom", "model":"small", "context_window":32768}}
        }))
        .unwrap();
        assert_eq!(
            cfg.resolve_subagent_context_window("custom", "small", Some("child")),
            32768
        );
        assert_eq!(
            cfg.resolve_subagent_context_window("foreign", "small", Some("child")),
            1_000_000
        );
        assert_eq!(cfg.context_window, Some(1_000_000));
        let again: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(again.resolve_subagent_max_tokens(), 32768);
        assert_eq!(again.resolve_task_max_report_bytes(), 524288);
        assert_eq!(again.resolve_task_report_repair_max_tokens(), 8192);
        assert_eq!(again.resolve_task_report_repair_max_cost_units(), 100000);
    }

    #[test]
    fn subagent127_invalid_settings_are_rejected_before_save_and_effects() {
        for (key, value) in [
            ("subagent_context_window", 4095),
            ("subagent_max_tokens", 0),
            ("task_max_report_bytes", 255),
            ("task_max_turns", 0),
            ("task_max_prompts", 0),
            ("task_max_concurrency", 0),
            ("task_max_output_bytes", 255),
            ("task_max_cost_units", 0),
            ("task_report_repair_max_tokens", 0),
            ("task_report_repair_max_cost_units", 0),
            ("task_timeout_secs", 86401),
        ] {
            let mut cfg = load::parse_config_content(
                std::path::Path::new("config.toml"),
                &format!("{key}={value}\n"),
            )
            .unwrap();
            let error = load::validate_extra_body(&mut cfg)
                .expect_err("invalid sizing must fail validation");
            assert!(error.contains(key), "{error}");
        }
        let cfg = Config {
            task_max_report_bytes: Some(9999),
            task_max_output_bytes: Some(1024),
            subagent_max_tokens: Some(1024),
            task_report_repair_max_tokens: Some(4096),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_task_max_report_bytes(), 1024);
        assert_eq!(cfg.resolve_task_report_repair_max_tokens(), 1024);
    }
}

#[cfg(all(test, feature = "subagents"))]
mod task_cost_config_tests {
    use super::{Config, TaskCostOverride, TaskCostWeighting, TaskCostWeights};

    #[test]
    fn task_cost_defaults_partial_objects_and_roundtrip_are_explicit() {
        let defaults = Config::default();
        assert_eq!(
            defaults.resolve_task_cost_weighting(),
            TaskCostWeighting::Auto
        );
        assert_eq!(
            defaults.resolve_task_cost_weights(),
            TaskCostWeights {
                input: 1.0,
                output: 5.0,
                cache_read: 1.0,
                cache_write_5m: 2.0,
                cache_write_1h: 2.0,
            }
        );
        let partial: Config = toml::from_str("task_cost_weighting='fixed'\n[task_cost_weights]\noutput=7.0\n[task_cost_overrides.child.weights]\ncache_read=0.0\n").unwrap();
        assert_eq!(
            partial.resolve_task_cost_weighting(),
            TaskCostWeighting::Fixed
        );
        assert_eq!(partial.resolve_task_cost_weights().input, 1.0);
        assert_eq!(partial.resolve_task_cost_weights().output, 7.0);
        let child = partial.task_cost_overrides.as_ref().unwrap()["child"];
        assert_eq!(child.weighting, None);
        assert_eq!(
            child.weights.unwrap(),
            TaskCostWeights {
                cache_read: 0.0,
                ..Default::default()
            }
        );
        let again: Config = toml::from_str(&toml::to_string(&partial).unwrap()).unwrap();
        assert_eq!(again.task_cost_weighting, partial.task_cost_weighting);
        assert_eq!(again.task_cost_weights, partial.task_cost_weights);
        assert_eq!(again.task_cost_overrides, partial.task_cost_overrides);
        let generated = super::load::rich_default_config();
        let again: Config = toml::from_str(&toml::to_string(&generated).unwrap()).unwrap();
        assert_eq!(again.task_cost_weighting, Some(TaskCostWeighting::Auto));
        assert_eq!(again.task_cost_weights, Some(TaskCostWeights::default()));
    }

    #[test]
    fn task_cost_rejects_unknown_fields_modes_and_blank_override_aliases() {
        for source in [
            "task_cost_weighting='automatic'",
            "task_cost_weights=[]",
            "task_cost_weights=[1.0, 5.0, 1.0, 2.0, 2.0]",
            "task_cost_overrides={child=[]}",
            "task_cost_overrides={child={weights=[]}}",
            "task_cost_weighting='Auto'",
            "[task_cost_weights]\noutput_typo=5.0",
            "[task_cost_overrides.child]\nunknown=true",
            "[task_cost_overrides.child]\nweighting='unknown'",
            "[task_cost_overrides.child.weights]\ncache_write=2.0",
        ] {
            assert!(toml::from_str::<Config>(source).is_err());
        }
        for value in [
            serde_json::json!([]),
            serde_json::json!([1, 5, 1, 2, 2]),
            serde_json::json!(3),
            serde_json::json!("weights"),
        ] {
            assert!(serde_json::from_value::<TaskCostWeights>(value.clone()).is_err());
            assert!(serde_json::from_value::<TaskCostOverride>(value).is_err());
        }
        let invalid = Config {
            task_cost_overrides: Some([(" ".into(), TaskCostOverride::default())].into()),
            ..Default::default()
        };
        assert!(
            invalid
                .validate_subagent_settings()
                .unwrap_err()
                .contains("task_cost_overrides")
        );
        // A staged alias is legal data; only the runtime's exact selected-route
        // check may decide whether an override applies.
        let staged: Config =
            toml::from_str("[task_cost_overrides.future]\nweighting='fixed'").unwrap();
        staged.validate_subagent_settings().unwrap();
    }

    #[test]
    fn task_cost_numeric_validation_covers_parsed_and_programmatic_values() {
        for field in [
            "input",
            "output",
            "cache_read",
            "cache_write_5m",
            "cache_write_1h",
        ] {
            for value in ["nan", "inf", "-inf", "-1.0"] {
                for prefix in ["task_cost_weights", "task_cost_overrides.child.weights"] {
                    let source = format!("[{prefix}]\n{field}={value}\n");
                    assert!(
                        toml::from_str::<Config>(&source).is_err(),
                        "{prefix}.{field}"
                    );
                }
            }
            let zero = format!("[task_cost_weights]\n{field}=0.0\n");
            assert_eq!(
                toml::from_str::<Config>(&zero).is_ok(),
                field.starts_with("cache_")
            );
        }
        for field in 0..5 {
            for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
                let mut weights = TaskCostWeights::default();
                match field {
                    0 => weights.input = value,
                    1 => weights.output = value,
                    2 => weights.cache_read = value,
                    3 => weights.cache_write_5m = value,
                    _ => weights.cache_write_1h = value,
                }
                for cfg in [
                    Config {
                        task_cost_weights: Some(weights),
                        ..Default::default()
                    },
                    Config {
                        task_cost_overrides: Some(
                            [(
                                "child".into(),
                                TaskCostOverride {
                                    weights: Some(weights),
                                    ..Default::default()
                                },
                            )]
                            .into(),
                        ),
                        ..Default::default()
                    },
                ] {
                    assert!(cfg.validate_subagent_settings().is_err());
                    assert!(
                        super::load::serialize_config_content(
                            std::path::Path::new("config.toml"),
                            &cfg
                        )
                        .is_err()
                    );
                }
            }
        }
    }
}

#[cfg(all(test, feature = "subagents"))]
mod agent_mention_tool_choice_tests {
    use super::{AgentMentionToolChoice, Config};

    #[test]
    fn agent_mention_tool_choice_defaults_to_hint_and_parses_force() {
        assert_eq!(
            Config::default().resolve_agent_mention_tool_choice(),
            AgentMentionToolChoice::Hint
        );
        let force: Config = toml::from_str("agent_mention_tool_choice='force'").unwrap();
        assert_eq!(
            force.resolve_agent_mention_tool_choice(),
            AgentMentionToolChoice::Force
        );
        let hint: Config = toml::from_str("agent_mention_tool_choice='hint'").unwrap();
        assert_eq!(
            hint.resolve_agent_mention_tool_choice(),
            AgentMentionToolChoice::Hint
        );
        let reparsed: Config = toml::from_str(&toml::to_string(&force).unwrap()).unwrap();
        assert_eq!(
            reparsed.agent_mention_tool_choice,
            force.agent_mention_tool_choice
        );
        for source in [
            "agent_mention_tool_choice='Force'",
            "agent_mention_tool_choice='always'",
            "agent_mention_tool_choice=true",
        ] {
            assert!(toml::from_str::<Config>(source).is_err(), "{source}");
        }
    }
}
