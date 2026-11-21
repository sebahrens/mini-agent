//! Model and reasoning-effort session config options for ACP editors.
//!
//! `model` lists the configured `[quick_models]` (each carries its provider),
//! then the baked catalogue models of providers with a usable credential, plus
//! `default`, which keeps the persona/startup route. `effort` lists the
//! reasoning levels the TUI's `/effort` accepts, plus `default`. Both are
//! per-session, in memory only, and take effect from the next prompt; they are
//! the same selections `/model` and `/effort` make as ACP slash commands.
//! A selection never widens authority: it chooses among models the operator
//! already configured, through the same credential check as a persona route.

use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
};

use crate::config::Config;
use crate::config::types::ReasoningEffort;

use super::persona::{TurnRoute, bounded_message};

pub(super) const MODEL_CONFIG_ID: &str = "model";
pub(super) const EFFORT_CONFIG_ID: &str = "effort";
/// The value that clears an override.
pub(super) const DEFAULT_VALUE: &str = "default";

/// At most this many models are advertised (quick models first); every
/// configured alias stays selectable by id, and so does every catalogue model
/// of a usable provider.
const MAX_ADVERTISED_MODELS: usize = 128;

/// Wire prefix of a catalogue selection: `catalog:<provider>/<model>`. The
/// stored override carries the same text, which no quick alias can (an alias
/// that starts with it is sent as `alias:<alias>` and wins when resolved).
const CATALOG_PREFIX: &str = "catalog:";

const EFFORTS: [ReasoningEffort; 5] = [
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::Xhigh,
    ReasoningEffort::Max,
];

/// One ACP session's model and effort selections; `None` is the default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Overrides {
    /// A `[quick_models]` alias.
    pub(super) model: Option<String>,
    pub(super) effort: Option<ReasoningEffort>,
}

/// Escape prefix of the wire encoding. Aliases that would collide with the
/// synthetic "clear override" value (`default`) or with the prefix itself are
/// sent as `alias:<alias>`, so the mapping alias -> wire value is injective:
/// a value without the prefix is a plain alias (never `default`, never
/// prefixed), and a value with it decodes by stripping exactly one prefix.
const RESERVED_ALIAS_PREFIX: &str = "alias:";

/// The value a client sends to select the quick-model `alias`.
fn wire_value(alias: &str) -> String {
    if alias == DEFAULT_VALUE
        || alias.starts_with(RESERVED_ALIAS_PREFIX)
        || alias.starts_with(CATALOG_PREFIX)
    {
        format!("{RESERVED_ALIAS_PREFIX}{alias}")
    } else {
        alias.to_owned()
    }
}

fn sorted_aliases(cfg: &Config) -> Vec<&String> {
    let mut aliases: Vec<&String> = crate::config::quick_models_map(cfg).keys().collect();
    aliases.sort();
    aliases
}

/// A catalogue model offered to the picker.
#[derive(Debug, PartialEq, Eq)]
struct CatalogChoice {
    provider: &'static str,
    model: String,
}

/// Baked catalogue models of providers `usable` accepts (a credential
/// resolves and the managed policy allows the provider), agent-capable only,
/// sorted by provider then id, at most `limit`.
fn catalog_choices(usable: impl Fn(&str) -> bool, limit: usize) -> Vec<CatalogChoice> {
    let mut providers = crate::models_catalog::catalog_provider_names();
    providers.sort_unstable();
    let mut out = Vec::new();
    for provider in providers {
        if out.len() >= limit {
            break;
        }
        let Some(entries) = crate::models_catalog::catalog_entries(provider) else {
            continue;
        };
        if !usable(provider) {
            continue;
        }
        let mut models: Vec<&str> = entries
            .iter()
            .filter(|entry| crate::provider::is_agent_model(entry))
            .map(|entry| entry.id.as_str())
            .collect();
        models.sort_unstable();
        for model in models.into_iter().take(limit - out.len()) {
            out.push(CatalogChoice {
                provider,
                model: model.to_owned(),
            });
        }
    }
    out
}

/// Whether a provider can serve a turn now: the managed policy allows it and
/// a credential resolves (the check `persona::check_route` applies).
fn provider_usable(cfg: &Config, provider: &str) -> bool {
    crate::config::managed::check_provider(provider).is_ok()
        && crate::provider::check_client_credentials(
            provider,
            None,
            cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .is_ok()
}

/// The `model` and `effort` config options reflecting `current`. The model
/// selector is omitted when `default` would be its only choice.
pub(super) fn config_options(cfg: &Config, current: &Overrides) -> Vec<SessionConfigOption> {
    config_options_with(cfg, current, |provider| provider_usable(cfg, provider))
}

fn config_options_with(
    cfg: &Config,
    current: &Overrides,
    usable: impl Fn(&str) -> bool,
) -> Vec<SessionConfigOption> {
    let mut out = Vec::new();
    let quick = crate::config::quick_models_map(cfg);
    let aliases: Vec<&String> = sorted_aliases(cfg)
        .into_iter()
        .take(MAX_ADVERTISED_MODELS)
        .collect();
    let catalog = catalog_choices(usable, MAX_ADVERTISED_MODELS - aliases.len());
    if !aliases.is_empty() || !catalog.is_empty() {
        let mut choices = vec![
            SessionConfigSelectOption::new(DEFAULT_VALUE, "Default")
                .description("The persona or startup model".to_string()),
        ];
        for alias in aliases {
            let entry = &quick[alias];
            choices.push(
                SessionConfigSelectOption::new(wire_value(alias), alias.to_string()).description(
                    bounded_message(&format!("{}/{}", entry.provider, entry.model)),
                ),
            );
        }
        for choice in &catalog {
            let name = format!("{}/{}", choice.provider, choice.model);
            choices.push(SessionConfigSelectOption::new(
                format!("{CATALOG_PREFIX}{name}"),
                bounded_message(&name),
            ));
        }
        let current_value = match current.model.as_deref() {
            None => DEFAULT_VALUE.to_owned(),
            // A catalogue selection is its own wire value; a quick alias is
            // escaped where it could be mistaken for one.
            Some(model) if !quick.contains_key(model) && model.starts_with(CATALOG_PREFIX) => {
                model.to_owned()
            }
            Some(model) => wire_value(model),
        };
        out.push(
            SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current_value, choices)
                .description(
                    "Model for this session's next prompts, from [quick_models] and the catalogue of providers with a credential"
                        .to_string(),
                )
                .category(SessionConfigOptionCategory::Model),
        );
    }
    let mut levels = vec![
        SessionConfigSelectOption::new(DEFAULT_VALUE, "Default")
            .description("The configured or provider default".to_string()),
    ];
    for level in EFFORTS {
        levels.push(SessionConfigSelectOption::new(
            level.as_wire_str(),
            level.as_wire_str(),
        ));
    }
    out.push(
        SessionConfigOption::select(
            EFFORT_CONFIG_ID,
            "Reasoning effort",
            current
                .effort
                .map_or(DEFAULT_VALUE, ReasoningEffort::as_wire_str)
                .to_string(),
            levels,
        )
        .description("Reasoning effort for this session's next prompts".to_string())
        .category(SessionConfigOptionCategory::ThoughtLevel),
    );
    out
}

/// Slash commands use the configured alias spelling; only picker IDs are escaped.
pub(super) fn command_model_value(value: &str) -> String {
    if value == DEFAULT_VALUE {
        value.to_owned()
    } else {
        wire_value(value)
    }
}

/// `default` clears the override; otherwise `value` must be a configured
/// quick-model alias or a `catalog:<provider>/<model>` entry of the baked
/// catalogue whose provider the managed policy allows.
pub(super) fn parse_model(cfg: &Config, value: &str) -> Result<Option<String>, String> {
    if value == DEFAULT_VALUE {
        return Ok(None);
    }
    // Decode before any lookup, so a configured alias that looks like an
    // escaped value can never shadow the alias the value actually names.
    let alias = value.strip_prefix(RESERVED_ALIAS_PREFIX).unwrap_or(value);
    if wire_value(alias) == value && crate::config::quick_models_map(cfg).contains_key(alias) {
        return Ok(Some(alias.to_owned()));
    }
    if let Some(name) = value.strip_prefix(CATALOG_PREFIX)
        && catalog_route(name).is_ok()
    {
        return Ok(Some(value.to_owned()));
    }
    Err(bounded_message(&format!(
        "unknown ACP model '{value}'; choose default, a configured quick model or a catalogue model"
    )))
}

/// The route `provider/model` names, when the baked catalogue lists the model
/// and the managed policy allows its provider. The credential is checked by
/// the caller like for any route.
fn catalog_route(name: &str) -> Result<TurnRoute, String> {
    let (provider, model) = name
        .split_once('/')
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .ok_or_else(|| bounded_message(&format!("ACP catalogue model '{name}' is malformed")))?;
    crate::config::managed::check_provider(provider)?;
    let listed = crate::models_catalog::catalog_entries(provider)
        .is_some_and(|entries| entries.iter().any(|entry| entry.id == model));
    if !listed {
        return Err(bounded_message(&format!(
            "ACP model '{name}' is not in the model catalogue"
        )));
    }
    Ok(TurnRoute {
        provider: provider.into(),
        model: model.into(),
        mapped: None,
    })
}

/// `default` clears the override; otherwise one of the advertised levels.
pub(super) fn parse_effort(value: &str) -> Result<Option<ReasoningEffort>, String> {
    if value == DEFAULT_VALUE {
        return Ok(None);
    }
    EFFORTS
        .into_iter()
        .find(|level| level.as_wire_str() == value)
        .map(Some)
        .ok_or_else(|| {
            bounded_message(&format!(
                "unknown ACP reasoning effort '{value}'; choose default, low, medium, high, xhigh or max"
            ))
        })
}

/// The turn route a model selection means: the alias's provider and model.
pub(super) fn route_for(cfg: &Config, alias: &str) -> Result<TurnRoute, String> {
    let quick = crate::config::quick_models_map(cfg);
    if !quick.contains_key(alias)
        && let Some(name) = alias.strip_prefix(CATALOG_PREFIX)
    {
        return catalog_route(name);
    }
    let entry = quick.get(alias).ok_or_else(|| {
        bounded_message(&format!(
            "ACP model '{alias}' is no longer a configured quick model"
        ))
    })?;
    Ok(TurnRoute {
        provider: entry.provider.clone(),
        model: entry.model.clone(),
        mapped: None,
    })
}

pub(super) fn effort_label(effort: Option<ReasoningEffort>) -> &'static str {
    effort.map_or(DEFAULT_VALUE, ReasoningEffort::as_wire_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::QuickModelConfig;

    fn cfg_with(quick: &[(&str, &str, &str)]) -> Config {
        Config {
            quick_models: Some(
                quick
                    .iter()
                    .map(|(alias, provider, model)| {
                        (
                            (*alias).to_owned(),
                            QuickModelConfig {
                                provider: (*provider).into(),
                                model: (*model).into(),
                                input_token_cost: None,
                                output_token_cost: None,
                                cache_read_token_cost: None,
                                cache_write_token_cost: None,
                                cache_write_1h_token_cost: None,
                                reserve_tokens: None,
                                temperature: None,
                                extra_body: None,
                                context_window: None,
                            },
                        )
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn model_option_lists_default_then_sorted_quick_models_with_providers() {
        let cfg = cfg_with(&[
            ("zeta", "openai", "gpt-z"),
            ("alpha", "anthropic", "claude-a"),
        ]);
        let options = config_options_with(&cfg, &Overrides::default(), |_| false);
        assert_eq!(options.len(), 2);
        let json = serde_json::to_value(&options[0]).unwrap();
        assert_eq!(json["id"], "model");
        assert_eq!(json["category"], "model");
        assert_eq!(json["currentValue"], "default");
        let values: Vec<_> = json["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(values, ["default", "alpha", "zeta"]);
        assert_eq!(json["options"][1]["description"], "anthropic/claude-a");
    }

    #[test]
    fn quick_model_named_default_is_selectable_under_a_distinct_value() {
        let cfg = cfg_with(&[("default", "openai", "gpt-d"), ("fast", "openai", "gpt-f")]);
        let json =
            serde_json::to_value(&config_options_with(&cfg, &Overrides::default(), |_| false)[0])
                .unwrap();
        let values: Vec<_> = json["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(values, ["default", "alias:default", "fast"]);
        assert_eq!(parse_model(&cfg, "default"), Ok(None));
        assert_eq!(
            parse_model(&cfg, "alias:default"),
            Ok(Some("default".into()))
        );
        let current = Overrides {
            model: Some("default".into()),
            effort: None,
        };
        let json =
            serde_json::to_value(&config_options_with(&cfg, &current, |_| false)[0]).unwrap();
        assert_eq!(json["currentValue"], "alias:default");
        let plain = cfg_with(&[("fast", "openai", "gpt-f")]);
        assert!(parse_model(&plain, "alias:default").is_err());
    }

    // mini-agent-xorxl.16
    #[test]
    fn reserved_prefix_aliases_get_unique_wire_values_that_round_trip() {
        let cfg = cfg_with(&[
            ("default", "p-default", "m-default"),
            ("alias:default", "p-prefixed", "m-prefixed"),
            ("alias:alias:default", "p-double", "m-double"),
            ("fast", "p-fast", "m-fast"),
        ]);
        let json =
            serde_json::to_value(&config_options_with(&cfg, &Overrides::default(), |_| false)[0])
                .unwrap();
        let values: Vec<String> = json["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap().to_owned())
            .collect();
        let unique: std::collections::HashSet<_> = values.iter().collect();
        assert_eq!(
            unique.len(),
            values.len(),
            "duplicate wire values: {values:?}"
        );
        assert_eq!(values.len(), 5);
        for (alias, provider) in [
            ("default", "p-default"),
            ("alias:default", "p-prefixed"),
            ("alias:alias:default", "p-double"),
            ("fast", "p-fast"),
        ] {
            let selected = parse_model(&cfg, &wire_value(alias)).unwrap().unwrap();
            assert_eq!(selected, alias);
            if alias != DEFAULT_VALUE {
                assert_eq!(
                    parse_model(&cfg, &command_model_value(alias)),
                    Ok(Some(alias.to_owned()))
                );
            }
            assert_eq!(&*route_for(&cfg, &selected).unwrap().provider, provider);
            let current = Overrides {
                model: Some(alias.into()),
                effort: None,
            };
            let json =
                serde_json::to_value(&config_options_with(&cfg, &current, |_| false)[0]).unwrap();
            assert_eq!(json["currentValue"], wire_value(alias));
        }
        assert_eq!(parse_model(&cfg, "default"), Ok(None));
        // Unescaped spellings of escaped aliases name nothing.
        assert!(parse_model(&cfg, "alias:alias:alias:alias:default").is_err());
    }

    #[test]
    fn effort_option_is_thought_level_and_model_option_absent_without_quick_models() {
        let options = config_options_with(
            &Config::default(),
            &Overrides {
                model: None,
                effort: Some(ReasoningEffort::High),
            },
            |_| false,
        );
        assert_eq!(options.len(), 1);
        let json = serde_json::to_value(&options[0]).unwrap();
        assert_eq!(json["id"], "effort");
        assert_eq!(json["category"], "thought_level");
        assert_eq!(json["currentValue"], "high");
    }

    #[test]
    fn parse_rejects_unknown_values_and_clears_on_default() {
        let cfg = cfg_with(&[("fast", "openai", "gpt-f")]);
        assert_eq!(parse_model(&cfg, "default"), Ok(None));
        assert_eq!(parse_model(&cfg, "fast"), Ok(Some("fast".into())));
        assert!(
            parse_model(&cfg, "gpt-f").is_err(),
            "raw model ids are not accepted"
        );
        assert_eq!(parse_effort("default"), Ok(None));
        assert_eq!(parse_effort("max"), Ok(Some(ReasoningEffort::Max)));
        assert!(parse_effort("none").is_err());
        assert!(parse_effort("turbo").is_err());
        assert_eq!(route_for(&cfg, "fast").unwrap().model, "gpt-f");
        assert!(route_for(&cfg, "gone").is_err());
    }

    #[test]
    fn catalogue_models_of_usable_providers_are_listed_after_quick_models() {
        let cfg = Config::default();
        let usable = |provider: &str| provider == "anthropic";
        let options = config_options_with(&cfg, &Overrides::default(), usable);
        let json = serde_json::to_value(&options[0]).unwrap();
        assert_eq!(json["id"], "model");
        let values: Vec<String> = json["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(values[0], "default");
        assert!(values.len() > 1 && values.len() <= MAX_ADVERTISED_MODELS + 1);
        assert!(
            values[1..]
                .iter()
                .all(|v| v.starts_with("catalog:anthropic/"))
        );
        // A selected value resolves to the expected route.
        let picked = values[1].clone();
        let selected = parse_model(&cfg, &picked).unwrap().unwrap();
        let route = route_for(&cfg, &selected).unwrap();
        assert_eq!(&*route.provider, "anthropic");
        assert_eq!(format!("catalog:anthropic/{}", route.model), picked);
        // The selection is echoed back as the current value.
        let current = Overrides {
            model: Some(selected),
            effort: None,
        };
        let json = serde_json::to_value(&config_options_with(&cfg, &current, usable)[0]).unwrap();
        assert_eq!(json["currentValue"], picked);
        // A provider that is not usable (no credential, or denied by the
        // managed policy) contributes nothing, and with no quick model the
        // selector is omitted.
        let none = config_options_with(&cfg, &Overrides::default(), |_| false);
        assert!(
            none.iter()
                .all(|o| serde_json::to_value(o).unwrap()["id"] != "model")
        );
    }

    #[test]
    fn catalogue_values_reject_unknown_providers_and_models() {
        let cfg = Config::default();
        assert!(parse_model(&cfg, "catalog:anthropic/not-a-model").is_err());
        assert!(parse_model(&cfg, "catalog:nope/gpt").is_err());
        assert!(parse_model(&cfg, "catalog:anthropic").is_err());
        assert!(parse_model(&cfg, "anthropic/claude").is_err());
        // A quick alias that looks like a catalogue value is escaped and wins.
        let quick = cfg_with(&[("catalog:x/y", "openai", "gpt-q")]);
        let wire = wire_value("catalog:x/y");
        assert_eq!(wire, "alias:catalog:x/y");
        assert_eq!(parse_model(&quick, &wire), Ok(Some("catalog:x/y".into())));
        assert_eq!(&*route_for(&quick, "catalog:x/y").unwrap().model, "gpt-q");
    }
}
