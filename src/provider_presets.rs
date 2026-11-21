//! Built-in presets for common OpenAI-compatible vendors (mini-agent-nvnkx.130).
//!
//! A preset is a `custom_providers` entry the user did not have to write. At
//! config load every preset whose name is not already a user entry is added to
//! the in-memory table, so `--provider groq`, `/provider`, the setup wizard and
//! the completion list all see it. Presets are credential-isolated exactly like
//! hand-written custom providers: they read only their own `api_key_env` or
//! `api_keys.<name>`, never the key of another vendor. They are not written
//! back when the config is saved, and a user entry of the same name wins.

use std::collections::HashMap;

use crate::config::{ApiStyle, Config, CustomProviderConfig};

/// One preset row. The embedded catalog has no tables for these vendors, so
/// their models are unpriced unless the user adds `model_prices`.
pub(crate) struct Preset {
    pub name: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    /// `None` for a keyless local server.
    pub api_key_env: Option<&'static str>,
}

pub(crate) const PRESETS: &[Preset] = &[
    Preset {
        name: "xai",
        label: "xAI",
        base_url: "https://api.x.ai/v1",
        api_key_env: Some("XAI_API_KEY"),
    },
    Preset {
        name: "groq",
        label: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        api_key_env: Some("GROQ_API_KEY"),
    },
    Preset {
        name: "mistral",
        label: "Mistral",
        base_url: "https://api.mistral.ai/v1",
        api_key_env: Some("MISTRAL_API_KEY"),
    },
    Preset {
        name: "deepseek",
        label: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        api_key_env: Some("DEEPSEEK_API_KEY"),
    },
    Preset {
        name: "together",
        label: "Together AI",
        base_url: "https://api.together.xyz/v1",
        api_key_env: Some("TOGETHER_API_KEY"),
    },
    Preset {
        name: "fireworks",
        label: "Fireworks AI",
        base_url: "https://api.fireworks.ai/inference/v1",
        api_key_env: Some("FIREWORKS_API_KEY"),
    },
    Preset {
        name: "cerebras",
        label: "Cerebras",
        base_url: "https://api.cerebras.ai/v1",
        api_key_env: Some("CEREBRAS_API_KEY"),
    },
    Preset {
        name: "lmstudio",
        label: "LM Studio (local)",
        base_url: "http://127.0.0.1:1234/v1",
        api_key_env: None,
    },
];

/// The preset called `name`, if any.
pub(crate) fn find(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

impl Preset {
    /// The `custom_providers` entry this preset expands to.
    pub(crate) fn expand(&self) -> CustomProviderConfig {
        let mut value = serde_json::json!({
            "provider_type": "openai",
            "base_url": self.base_url,
            "api_style": "completions",
        });
        if let Some(env) = self.api_key_env {
            value["api_key_env"] = env.into();
        }
        let mut entry: CustomProviderConfig =
            serde_json::from_value(value).expect("preset table is valid");
        debug_assert_eq!(entry.api_style, Some(ApiStyle::Completions));
        entry.inherit_builtin_key = false;
        entry
    }
}

/// Add every preset the user has not defined to `cfg.custom_providers`.
pub(crate) fn apply(cfg: &mut Config) {
    let map = cfg.custom_providers.get_or_insert_with(HashMap::new);
    for preset in PRESETS {
        map.entry(preset.name.to_owned())
            .or_insert_with(|| preset.expand());
    }
}

/// Whether `entry` is exactly the untouched expansion of the preset `name`;
/// such entries are not persisted because load recreates them.
pub(crate) fn is_untouched(name: &str, entry: &CustomProviderConfig) -> bool {
    find(name).is_some_and(|preset| {
        serde_json::to_value(preset.expand()).ok() == serde_json::to_value(entry).ok()
    })
}

/// `serialize_with` for `Config::custom_providers`: omit untouched presets.
pub(crate) fn serialize_custom_providers<S: serde::Serializer>(
    map: &Option<HashMap<String, CustomProviderConfig>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let entries: Vec<_> = map
        .iter()
        .flatten()
        .filter(|(name, entry)| !is_untouched(name, entry))
        .collect();
    let mut out = serializer.serialize_map(Some(entries.len()))?;
    for (name, entry) in entries {
        out.serialize_entry(name, entry)?;
    }
    out.end()
}

/// `skip_serializing_if` companion: nothing left to persist.
pub(crate) fn nothing_to_persist(map: &Option<HashMap<String, CustomProviderConfig>>) -> bool {
    map.iter()
        .flatten()
        .all(|(name, entry)| is_untouched(name, entry))
}

/// Markdown table for docs/agent/PROVIDERS.md, generated from [`PRESETS`].
#[cfg(test)]
pub(crate) fn markdown_table() -> String {
    let mut out = String::from(
        "| Vendor | `--provider` name | Base URL | Default env var for API key |\n| --- | --- | --- | --- |\n",
    );
    for p in PRESETS {
        out.push_str(&format!(
            "| {} | `{}` | `{}` | {} |\n",
            p.label,
            p.name,
            p.base_url,
            p.api_key_env
                .map(|e| format!("`{e}`"))
                .unwrap_or_else(|| "(no key; loopback)".into())
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ProviderKind;

    #[test]
    fn every_preset_resolves_and_is_credential_isolated() {
        let mut cfg = Config::default();
        apply(&mut cfg);
        let customs = cfg.custom_providers_map();
        for p in PRESETS {
            assert!(
                ProviderKind::from_name(p.name).is_none(),
                "{} shadows a native provider",
                p.name
            );
            let entry = customs.get(p.name).expect("preset applied");
            assert!(!entry.inherit_builtin_key);
            assert_eq!(entry.provider_type.as_str(), "openai");
            assert_eq!(entry.base_url, p.base_url);
            assert_eq!(entry.api_key_env.as_deref(), p.api_key_env);
            let resolved = crate::provider::resolve_provider_config(p.name, customs)
                .unwrap_or_else(|e| panic!("{}: {e}", p.name));
            assert_eq!(resolved.base_url.as_deref(), Some(p.base_url));
            if let Some(env) = p.api_key_env {
                assert_ne!(env, "OPENAI_API_KEY");
            }
        }
    }

    #[test]
    fn user_entry_wins_and_presets_are_not_persisted() {
        let mut cfg: Config = toml::from_str(
            "[custom_providers.groq]\nprovider_type = 'openai'\nbase_url = 'https://proxy.example/v1'\n",
        )
        .unwrap();
        apply(&mut cfg);
        assert_eq!(
            cfg.custom_providers_map()["groq"].base_url,
            "https://proxy.example/v1"
        );
        assert!(cfg.custom_providers_map().contains_key("xai"));
        let saved = toml::to_string(&cfg).unwrap();
        assert!(saved.contains("proxy.example"), "{saved}");
        assert!(!saved.contains("api.x.ai"), "{saved}");
        let mut only = Config::default();
        apply(&mut only);
        assert!(!toml::to_string(&only).unwrap().contains("custom_providers"));
    }

    #[test]
    fn docs_table_matches_the_preset_data() {
        let docs = include_str!("../docs/agent/PROVIDERS.md");
        assert!(
            docs.contains(markdown_table().trim_end()),
            "regenerate the preset table in docs/agent/PROVIDERS.md:\n{}",
            markdown_table()
        );
    }
}
