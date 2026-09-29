use std::collections::HashMap;
use std::env::VarError;

/// Kind of AI provider
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenRouter,
    OpenAI,
    Anthropic,
    Gemini,
    Ollama,
}

impl ProviderKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "openrouter" => Some(Self::OpenRouter),
            "openai" | "custom" => Some(Self::OpenAI), // "custom" is an alias for OpenAI client
            "anthropic" => Some(Self::Anthropic),
            "gemini" | "google" => Some(Self::Gemini),
            "ollama" => Some(Self::Ollama),
            _ => None,
        }
    }
}

/// Resolver for API keys with priority: CLI arg > env var > config file > custom provider name
#[derive(Debug, Clone)]
pub struct AuthResolver {
    pub provider_kind: ProviderKind,
    pub api_key_env_override: Option<String>,
    pub cli_key: Option<String>,
    pub config_api_keys: Option<HashMap<String, String>>,
    /// Custom provider name (e.g., "local-vllm") for fallback key lookup
    pub custom_provider_name: Option<String>,
    /// Explicit credential-isolation decision for `custom_provider_name`.
    /// `None` isolates exactly the names that are not built-in aliases;
    /// `Some(true)` isolates even a built-in alias (a `custom_providers`
    /// entry that shadows `openai`, `google`, `custom`, ... with a
    /// third-party `base_url`); `Some(false)` lets the name fall back to
    /// the built-in kind's env var and `api_keys` slot.
    pub credential_isolation: Option<bool>,
}

impl AuthResolver {
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            provider_kind: kind,
            api_key_env_override: None,
            cli_key: None,
            config_api_keys: None,
            custom_provider_name: None,
            credential_isolation: None,
        }
    }

    pub fn with_cli_key(mut self, key: Option<&str>) -> Self {
        self.cli_key = key.filter(|k| !k.is_empty()).map(String::from);
        self
    }

    pub fn with_env_override(mut self, env_var: Option<&str>) -> Self {
        self.api_key_env_override = env_var.filter(|s| !s.is_empty()).map(String::from);
        self
    }

    pub fn with_config_keys(mut self, keys: Option<&HashMap<String, String>>) -> Self {
        self.config_api_keys = keys.cloned();
        self
    }

    pub fn with_custom_provider_name(mut self, name: Option<&str>) -> Self {
        self.custom_provider_name = name.filter(|s| !s.is_empty()).map(String::from);
        self
    }

    /// Overrides the name-based credential-isolation default; see
    /// [`AuthResolver::credential_isolation`].
    pub fn with_credential_isolation(mut self, isolated: Option<bool>) -> Self {
        self.credential_isolation = isolated;
        self
    }

    pub fn resolve(&self) -> anyhow::Result<String> {
        self.resolve_with_env(|name| std::env::var(name))
    }

    /// A custom provider is any name that is not a built-in provider alias,
    /// plus any built-in alias the caller explicitly isolates (a
    /// `custom_providers` entry that shadows a built-in name with a
    /// third-party `base_url`). Its credentials are isolated: only its own
    /// `api_key_env` and its own `api_keys[<name>]` entry are consulted, never
    /// the built-in kind's environment variable or config slot, so a real
    /// vendor key is never sent to a third-party `base_url` that merely speaks
    /// the same protocol.
    fn custom_provider(&self) -> Option<&str> {
        self.custom_provider_name.as_deref().filter(|name| {
            self.credential_isolation
                .unwrap_or_else(|| ProviderKind::from_name(name).is_none())
        })
    }

    pub fn resolve_with_env<F: Fn(&str) -> Result<String, VarError>>(
        &self,
        get_env: F,
    ) -> anyhow::Result<String> {
        let custom = self.custom_provider();

        // Priority 1: CLI argument
        if let Some(ref key) = self.cli_key {
            tracing::warn!(
                "API key provided via --api-key is visible in process listings. \
                 Use the {} environment variable instead.",
                self.api_key_env_override
                    .as_deref()
                    .unwrap_or_else(|| self.env_var_name())
            );
            return Ok(key.clone());
        }

        // Priority 2: Environment variable. Custom providers only ever read
        // their explicitly configured `api_key_env`.
        let env_var = match (custom, self.api_key_env_override.as_deref()) {
            (_, Some(configured)) => Some(configured),
            (Some(_), None) => None,
            (None, None) => Some(self.env_var_name()),
        };
        if let Some(env_var) = env_var
            && let Ok(key) = get_env(env_var)
            && !key.is_empty()
        {
            return Ok(key);
        }

        // Priority 3: Config file. Built-ins use their slug (and, for
        // compatibility, the name they were referenced by); custom providers
        // use exactly their own name, unless that name is itself a vendor's
        // `api_keys` slot (an isolated `openai`, `google`, ... entry), which
        // holds the vendor key rather than one for the third-party gateway.
        let own_config_slot = custom.filter(|name| !is_vendor_key_slot(name));
        if let Some(ref keys) = self.config_api_keys {
            let lookup = |name: &str| keys.get(name).filter(|k| !k.is_empty()).cloned();
            match custom {
                Some(_) => {
                    if let Some(name) = own_config_slot
                        && let Some(key) = lookup(name)
                    {
                        return Ok(key);
                    }
                }
                None => {
                    if let Some(key) = lookup(self.provider_slug()) {
                        return Ok(key);
                    }
                    if let Some(name) = self.custom_provider_name.as_deref()
                        && let Some(key) = lookup(name)
                    {
                        return Ok(key);
                    }
                }
            }
        }

        // Ollama doesn't require an API key
        if self.provider_kind == ProviderKind::Ollama {
            return Ok(String::new());
        }

        match custom {
            Some(name) => anyhow::bail!(
                "No API key found for custom provider '{name}'. {}{}pass --api-key, or run `mini-agent --setup` to configure interactively. Built-in vendor credentials are never sent to a custom provider's base_url unless its custom_providers entry sets `inherit_builtin_key = true`.",
                match env_var {
                    Some(env_var) => format!("Set the {env_var} environment variable, "),
                    None => format!(
                        "Set `api_key_env` in custom_providers.{name} and export that variable, "
                    ),
                },
                match own_config_slot {
                    Some(name) => format!("add it to config.api_keys under '{name}', "),
                    None => String::new(),
                }
            ),
            None => anyhow::bail!(
                "No API key found. Set the {} environment variable, add it to config.api_keys under '{}', pass --api-key, or run `mini-agent --setup` to configure interactively.",
                env_var.unwrap_or_else(|| self.env_var_name()),
                self.provider_slug(),
            ),
        }
    }

    fn env_var_name(&self) -> &'static str {
        self.provider_kind.env_var_name()
    }

    fn provider_slug(&self) -> &'static str {
        self.provider_kind.slug()
    }
}

/// Whether `name` is an `api_keys` slot a built-in provider reads its vendor
/// key from (every built-in alias except the generic `custom`).
fn is_vendor_key_slot(name: &str) -> bool {
    ProviderKind::from_name(name).is_some() && !name.eq_ignore_ascii_case("custom")
}

impl ProviderKind {
    /// The vendor environment variable holding this kind's API key.
    pub fn env_var_name(self) -> &'static str {
        match self {
            ProviderKind::OpenAI => "OPENAI_API_KEY",
            ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
            ProviderKind::Gemini => "GEMINI_API_KEY",
            ProviderKind::Ollama => "OLLAMA_API_KEY",
            ProviderKind::OpenRouter => "OPENROUTER_API_KEY",
        }
    }

    /// The vendor's own API endpoint for this kind. A `custom_providers`
    /// entry that shadows a built-in name keeps that kind's vendor
    /// credentials only when its `base_url` has this origin.
    pub fn builtin_endpoint(self) -> &'static str {
        match self {
            ProviderKind::OpenAI => "https://api.openai.com/v1",
            ProviderKind::Anthropic => "https://api.anthropic.com",
            ProviderKind::Gemini => "https://generativelanguage.googleapis.com",
            ProviderKind::Ollama => "http://localhost:11434",
            ProviderKind::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }

    /// Whether `base_url` addresses this kind's own vendor endpoint (same
    /// scheme, host and port as [`ProviderKind::builtin_endpoint`]).
    pub fn is_builtin_endpoint(self, base_url: &str) -> bool {
        let (Ok(candidate), Ok(builtin)) = (
            reqwest::Url::parse(base_url.trim()),
            reqwest::Url::parse(self.builtin_endpoint()),
        ) else {
            return false;
        };
        candidate.origin() == builtin.origin()
    }

    /// The canonical `api_keys` slot for this kind.
    pub fn slug(self) -> &'static str {
        match self {
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::OpenAI => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Ollama => "ollama",
        }
    }
}
