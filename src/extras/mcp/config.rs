use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Whether this server's schemas enter the initial model request. Discovery
/// changes advertisement only; it never grants permission to call a tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McpDefer {
    Eager,
    Deferred,
    #[default]
    Auto,
}

impl McpDefer {
    pub(crate) const AUTO_THRESHOLD: usize = 32;

    pub(crate) const fn is_auto(value: &Self) -> bool {
        matches!(value, Self::Auto)
    }

    pub(crate) const fn resolves_deferred(self, total_tools: usize) -> bool {
        match self {
            Self::Eager => false,
            Self::Deferred => true,
            Self::Auto => total_tools > Self::AUTO_THRESHOLD,
        }
    }
}

impl Serialize for McpDefer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Eager => serializer.serialize_bool(false),
            Self::Deferred => serializer.serialize_bool(true),
            Self::Auto => serializer.serialize_str("auto"),
        }
    }
}

impl<'de> Deserialize<'de> for McpDefer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Boolean(bool),
            Text(String),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Boolean(false) => Ok(Self::Eager),
            Wire::Boolean(true) => Ok(Self::Deferred),
            Wire::Text(value) if value == "auto" => Ok(Self::Auto),
            _ => Err(serde::de::Error::custom(
                "MCP defer must be true, false or auto",
            )),
        }
    }
}

/// Immutable identity for an MCP server registered by mini-agent itself.
///
/// Its value is opaque outside this module, its constructors are crate-private,
/// and the corresponding config variant is skipped by serde, so user
/// configuration cannot manufacture a trusted registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedMcpServer(TrustedMcpServerKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrustedMcpServerKind {
    Exa,
    Context7,
    GrepApp,
}

impl TrustedMcpServer {
    pub(crate) const EXA: Self = Self(TrustedMcpServerKind::Exa);
    pub(crate) const CONTEXT7: Self = Self(TrustedMcpServerKind::Context7);
    pub(crate) const GREP_APP: Self = Self(TrustedMcpServerKind::GrepApp);

    pub(crate) const fn endpoint(self) -> &'static str {
        match self.0 {
            TrustedMcpServerKind::Exa => "https://mcp.exa.ai/mcp",
            TrustedMcpServerKind::Context7 => "https://mcp.context7.com/mcp",
            TrustedMcpServerKind::GrepApp => "https://mcp.grep.app",
        }
    }

    /// Exact read-only tool allowlist for trusted built-in registrations.
    ///
    /// These tools only retrieve public web, documentation, or source-search
    /// results. Unknown or newly added server tools must use normal permission
    /// rules until they are deliberately reviewed and added here.
    pub(crate) fn exempts_read_only_tool(self, tool_name: &str) -> bool {
        match self.0 {
            TrustedMcpServerKind::Exa => matches!(tool_name, "websearch" | "webfetch"),
            TrustedMcpServerKind::Context7 => {
                matches!(tool_name, "get_context" | "search_docs")
            }
            TrustedMcpServerKind::GrepApp => {
                matches!(tool_name, "search_code" | "search_repos")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    Command {
        command: String,
        #[serde(default, skip_serializing_if = "McpDefer::is_auto")]
        defer: McpDefer,
        #[serde(default)]
        args: Vec<String>,
        /// Working directory for the service. When omitted, the connection
        /// workspace is captured and applied explicitly before spawn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        /// Values supplied directly to the service after clearing the ambient
        /// process environment.
        #[serde(default)]
        env: HashMap<String, String>,
        /// Parent environment names deliberately delegated to the service.
        /// Explicit `env` values win when a name appears in both collections.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inherit_env: Vec<String>,
        /// Optional dedicated workspace-service sandbox backend. Omitting it
        /// is an explicit trusted-code bypass, not an implicit sandbox claim.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox: Option<String>,
        /// Network authority requested for this service process.
        #[serde(default, skip_serializing_if = "McpStdioNetwork::is_inherit")]
        network: McpStdioNetwork,
        /// Per-server timeouts and tool selection.
        #[serde(flatten, default)]
        options: Box<McpServerOptions>,
    },
    Url {
        url: String,
        #[serde(default, skip_serializing_if = "McpDefer::is_auto")]
        defer: McpDefer,
        /// Explicitly permit only pinned loopback endpoints, never private LANs.
        #[serde(default, skip_serializing_if = "is_false")]
        allow_local: bool,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<OAuthConfig>,
        /// Per-server timeouts and tool selection.
        #[serde(flatten, default)]
        options: Box<McpServerOptions>,
    },
    /// A built-in registration whose endpoint and trust identity cannot be
    /// supplied or changed through deserialized user configuration.
    #[serde(skip)]
    BuiltIn {
        identity: TrustedMcpServer,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// Accepted per-server timeout range in seconds; values are clamped into it.
pub(crate) const MCP_SERVER_TIMEOUT_SECS_RANGE: std::ops::RangeInclusive<u64> = 1..=3600;

/// Per-server operational settings shared by command and URL servers. Every
/// field is optional and omitted from serialization when unset, so existing
/// configurations (and their project source proofs) are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerOptions {
    /// Bound on connect + initialize (and, for HTTP, OAuth restore). It also
    /// bounds the server's `tools/list` enumeration. Defaults: 10 s / 30 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_sec: Option<u64>,
    /// Bound on one `tools/call` for this server, overriding the global
    /// `mcp_tool_timeout_secs` (default 120 s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_sec: Option<u64>,
    /// `false` keeps the entry configured but never connects it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// When set, only these server tool names are registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// Server tool names never registered. Applied after `enabled_tools`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<String>,
    /// Opt in to running this server's tools that advertise
    /// `readOnlyHint: true` on the shared (parallel) tool lane. Off by
    /// default: a server's hint is its own claim, so by default every MCP tool
    /// takes the exclusive lane.
    #[serde(default, skip_serializing_if = "is_false")]
    pub parallel_read_only: bool,
    /// URL servers only: a command run at connect time whose JSON output
    /// supplies extra request headers (for example a short-lived token).
    /// It never runs unless the top-level `mcp_headers_helper = true` opt-in
    /// is set; see `headers_helper.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers_helper: Option<McpHeadersHelper>,
}

/// The command behind `headers_helper`. Never run through a shell: `command`
/// is an argument vector whose first element is the program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "McpHeadersHelperWire")]
pub struct McpHeadersHelper {
    pub command: Vec<String>,
    /// Parent environment names delegated to the helper; everything else is
    /// cleared. Empty by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherit_env: Vec<String>,
    /// Optional workspace-service sandbox backend, as for stdio servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum McpHeadersHelperWire {
    Program(String),
    Argv(Vec<String>),
    Full {
        command: Vec<String>,
        #[serde(default)]
        inherit_env: Vec<String>,
        #[serde(default)]
        sandbox: Option<String>,
    },
}

impl From<McpHeadersHelperWire> for McpHeadersHelper {
    fn from(wire: McpHeadersHelperWire) -> Self {
        match wire {
            McpHeadersHelperWire::Program(program) => Self {
                command: vec![program],
                inherit_env: Vec::new(),
                sandbox: None,
            },
            McpHeadersHelperWire::Argv(command) => Self {
                command,
                inherit_env: Vec::new(),
                sandbox: None,
            },
            McpHeadersHelperWire::Full {
                command,
                inherit_env,
                sandbox,
            } => Self {
                command,
                inherit_env,
                sandbox,
            },
        }
    }
}

impl McpHeadersHelper {
    /// Structural bounds, checked when a definition is parsed.
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.command.is_empty() || self.command[0].trim().is_empty() {
            return Err("headersHelper needs a program");
        }
        if self.command.len() > 16 || self.command.iter().any(|arg| arg.len() > 4096) {
            return Err("headersHelper command exceeds bounds");
        }
        if self.inherit_env.len() > 32 {
            return Err("headersHelper inherit_env exceeds bounds");
        }
        Ok(())
    }
}

fn clamp_timeout(secs: u64) -> std::time::Duration {
    std::time::Duration::from_secs(secs.clamp(
        *MCP_SERVER_TIMEOUT_SECS_RANGE.start(),
        *MCP_SERVER_TIMEOUT_SECS_RANGE.end(),
    ))
}

impl McpServerOptions {
    /// Initialization bound: the configured value or `default`.
    pub(crate) fn startup_timeout(&self, default: std::time::Duration) -> std::time::Duration {
        self.startup_timeout_sec.map_or(default, clamp_timeout)
    }

    /// `tools/list` bound: a configured startup timeout raises (never lowers)
    /// the list budget, so a slow server given more time to start also gets
    /// it to enumerate.
    pub(crate) fn list_timeout(&self, default: std::time::Duration) -> std::time::Duration {
        self.startup_timeout_sec
            .map_or(default, |secs| clamp_timeout(secs).max(default))
    }

    /// Whether the entry should be connected at all (default `true`).
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled != Some(false)
    }

    /// Whether a tool with this exact server-side name may be registered.
    /// The selection applies before the per-server tool cap.
    pub(crate) fn admits_tool(&self, name: &str) -> bool {
        self.enabled_tools
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|tool| tool == name))
            && !self.disabled_tools.iter().any(|tool| tool == name)
    }

    /// `tools/call` bound: the configured value or `default`.
    pub(crate) fn tool_timeout(&self, default: std::time::Duration) -> std::time::Duration {
        self.tool_timeout_sec.map_or(default, clamp_timeout)
    }
}

/// Network authority is a launch property, independent from permission to
/// invoke any tool later exposed by the MCP server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpStdioNetwork {
    /// Use the host network when unsandboxed, or retain network access in a
    /// requested workspace-service sandbox.
    #[default]
    Inherit,
    /// Require the selected sandbox backend to deny network access. This is
    /// rejected when no sandbox is selected or the backend cannot enforce it.
    Deny,
}

impl McpStdioNetwork {
    const fn is_inherit(value: &Self) -> bool {
        matches!(value, Self::Inherit)
    }
}

impl McpServerConfig {
    pub(crate) const fn defer(&self) -> McpDefer {
        match self {
            Self::Command { defer, .. } | Self::Url { defer, .. } => *defer,
            Self::BuiltIn { .. } => McpDefer::Auto,
        }
    }

    pub(crate) fn built_in(identity: TrustedMcpServer, headers: HashMap<String, String>) -> Self {
        Self::BuiltIn { identity, headers }
    }

    /// Per-server options; built-in registrations use the defaults.
    pub(crate) fn options(&self) -> McpServerOptions {
        match self {
            Self::Command { options, .. } | Self::Url { options, .. } => (**options).clone(),
            Self::BuiltIn { .. } => McpServerOptions::default(),
        }
    }

    pub(crate) const fn trusted_identity(&self) -> Option<TrustedMcpServer> {
        match self {
            Self::BuiltIn { identity, .. } => Some(*identity),
            Self::Command { .. } | Self::Url { .. } => None,
        }
    }
}

/// OAuth settings for a URL-based MCP server.
///
/// Accepts either a bare `true` (enable with all defaults: dynamic client
/// registration, no extra scopes) or an object with explicit fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OAuthConfig {
    Enabled(bool),
    Settings(OAuthSettings),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OAuthSettings {
    /// OAuth scopes to request. Empty means none are requested explicitly.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Pre-registered client id. When absent, dynamic client registration is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Loopback port for the redirect URI. Defaults to [`DEFAULT_REDIRECT_PORT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_port: Option<u16>,
    /// Host spelling used in the registered redirect URI. The listener still
    /// binds only to IPv4 loopback. Defaults to `127.0.0.1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_host: Option<OAuthRedirectHost>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OAuthRedirectHost {
    #[serde(rename = "127.0.0.1")]
    Ipv4Loopback,
    #[serde(rename = "localhost")]
    Localhost,
}

pub const DEFAULT_REDIRECT_PORT: u16 = 8970;

impl OAuthConfig {
    /// Returns the resolved settings if OAuth is enabled, or `None` if disabled.
    pub fn settings(&self) -> Option<OAuthSettings> {
        match self {
            OAuthConfig::Enabled(false) => None,
            OAuthConfig::Enabled(true) => Some(OAuthSettings::default()),
            OAuthConfig::Settings(s) => Some(s.clone()),
        }
    }
}

impl OAuthSettings {
    pub fn redirect_port(&self) -> u16 {
        self.redirect_port.unwrap_or(DEFAULT_REDIRECT_PORT)
    }

    pub fn redirect_uri(&self) -> String {
        let host = match self
            .redirect_host
            .unwrap_or(OAuthRedirectHost::Ipv4Loopback)
        {
            OAuthRedirectHost::Ipv4Loopback => "127.0.0.1",
            OAuthRedirectHost::Localhost => "localhost",
        };
        format!("http://{host}:{}/callback", self.redirect_port())
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
mod options_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parallel_read_only_is_default_closed_and_opt_in() {
        let plain: McpServerConfig =
            serde_json::from_value(serde_json::json!({"command": "server"})).unwrap();
        assert!(!plain.options().parallel_read_only);
        assert!(
            !serde_json::to_string(&plain)
                .unwrap()
                .contains("parallel_read_only")
        );
        let opted: McpServerConfig = serde_json::from_value(
            serde_json::json!({"url": "https://mcp.example/mcp", "parallel_read_only": true}),
        )
        .unwrap();
        assert!(opted.options().parallel_read_only);
    }

    #[test]
    fn mcp_server_options_deserialize_flattened_and_clamp() {
        let config: McpServerConfig = serde_json::from_value(serde_json::json!({
            "command": "server", "startup_timeout_sec": 30, "tool_timeout_sec": 0
        }))
        .unwrap();
        let options = config.options();
        assert_eq!(options.startup_timeout_sec, Some(30));
        let default = Duration::from_secs(10);
        assert_eq!(options.startup_timeout(default), Duration::from_secs(30));
        assert_eq!(
            options.list_timeout(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        assert_eq!(
            options.list_timeout(Duration::from_secs(5)),
            Duration::from_secs(30)
        );
        assert_eq!(
            options.tool_timeout(Duration::from_secs(120)),
            Duration::from_secs(1)
        );
        let url: McpServerConfig = serde_json::from_value(serde_json::json!({
            "url": "https://mcp.example/mcp", "startup_timeout_sec": 99999
        }))
        .unwrap();
        assert_eq!(
            url.options().startup_timeout(default),
            Duration::from_secs(3600)
        );
        // Unset options keep every default and serialize to nothing new.
        let plain: McpServerConfig =
            serde_json::from_value(serde_json::json!({"command": "server"})).unwrap();
        assert_eq!(plain.options(), McpServerOptions::default());
        assert_eq!(plain.options().startup_timeout(default), default);
        let text = serde_json::to_string(&plain).unwrap();
        assert!(!text.contains("timeout"), "{text}");
        assert!(!text.contains("enabled"), "{text}");
    }

    #[test]
    fn mcp_server_tool_selection_and_enable_toggle() {
        let config: McpServerConfig = serde_json::from_value(serde_json::json!({
            "command": "server", "enabled_tools": ["a", "b"], "disabled_tools": ["b"]
        }))
        .unwrap();
        let options = config.options();
        assert!(options.is_enabled());
        assert!(options.admits_tool("a"));
        assert!(!options.admits_tool("b"));
        assert!(!options.admits_tool("c"));
        let off: McpServerConfig = serde_json::from_value(serde_json::json!({
            "url": "https://mcp.example/mcp", "enabled": false
        }))
        .unwrap();
        assert!(!off.options().is_enabled());
        assert!(McpServerOptions::default().admits_tool("anything"));
    }
}
