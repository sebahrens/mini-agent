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
