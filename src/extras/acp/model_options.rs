//! Model and reasoning-effort session config options for ACP editors.
//!
//! `model` lists the configured `[quick_models]` (each carries its provider)
//! plus `default`, which keeps the persona/startup route. `effort` lists the
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

/// At most this many quick models are advertised; every configured alias
/// stays selectable by id.
const MAX_ADVERTISED_MODELS: usize = 128;

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

fn sorted_aliases(cfg: &Config) -> Vec<&String> {
    let mut aliases: Vec<&String> = crate::config::quick_models_map(cfg).keys().collect();
    aliases.sort();
    aliases
}

/// The `model` and `effort` config options reflecting `current`. The model
/// selector is omitted when no quick model is configured, since `default`
/// would be its only choice.
pub(super) fn config_options(cfg: &Config, current: &Overrides) -> Vec<SessionConfigOption> {
    let mut out = Vec::new();
    let quick = crate::config::quick_models_map(cfg);
    if !quick.is_empty() {
        let mut choices = vec![
            SessionConfigSelectOption::new(DEFAULT_VALUE, "Default")
                .description("The persona or startup model".to_string()),
        ];
        for alias in sorted_aliases(cfg).into_iter().take(MAX_ADVERTISED_MODELS) {
            let entry = &quick[alias];
            choices.push(
                SessionConfigSelectOption::new(alias.to_string(), alias.to_string()).description(
                    bounded_message(&format!("{}/{}", entry.provider, entry.model)),
                ),
            );
        }
        out.push(
            SessionConfigOption::select(
                MODEL_CONFIG_ID,
                "Model",
                current
                    .model
                    .as_deref()
                    .unwrap_or(DEFAULT_VALUE)
                    .to_string(),
                choices,
            )
            .description("Model for this session's next prompts, from [quick_models]".to_string())
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

/// `default` clears the override; otherwise `value` must be a configured
/// quick-model alias.
pub(super) fn parse_model(cfg: &Config, value: &str) -> Result<Option<String>, String> {
    if value == DEFAULT_VALUE {
        return Ok(None);
    }
    if crate::config::quick_models_map(cfg).contains_key(value) {
        return Ok(Some(value.to_owned()));
    }
    Err(bounded_message(&format!(
        "unknown ACP model '{value}'; choose default or a configured quick model"
    )))
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
    let entry = crate::config::quick_models_map(cfg)
        .get(alias)
        .ok_or_else(|| {
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
        let options = config_options(&cfg, &Overrides::default());
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
    fn effort_option_is_thought_level_and_model_option_absent_without_quick_models() {
        let options = config_options(
            &Config::default(),
            &Overrides {
                model: None,
                effort: Some(ReasoningEffort::High),
            },
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
}
