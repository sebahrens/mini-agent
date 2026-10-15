//! Known-key settings picker. Values stay in the input draft until explicitly submitted.
use super::{handlers, list::ListPicker};
use crate::config::{
    Config,
    editor::Target,
    keys::{self, ConfigShape},
};
use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) struct ConfigPicker {
    pub(crate) list: ListPicker,
    prefix: String,
    target: Target,
    keys: bool,
}
impl ConfigPicker {
    pub(crate) fn new(
        cfg: &Config,
        cli: &crate::cli::Cli,
        target: Target,
        key: Option<&str>,
    ) -> Self {
        let mut this = Self {
            list: ListPicker::new(),
            prefix: target.prefix().into(),
            target,
            keys: true,
        };
        if let Some(key) = key {
            this.choose(key);
        } else {
            let values = crate::config::layers::normalized_json(cfg).ok();
            #[cfg(feature = "subagents")]
            let live_limits = crate::extras::subagents::live_limit_values();
            let entries = keys::CONFIG_KEYS
                .iter()
                .filter(|key| key.feature.is_none_or(keys::feature_enabled))
                .map(|key| {
                    let dedicated = dedicated_value(cli, key.name);
                    #[cfg(feature = "subagents")]
                    let live_limit = live_limits.as_ref().and_then(|values| values.get(key.name));
                    #[cfg(not(feature = "subagents"))]
                    let live_limit: Option<&serde_json::Value> = None;
                    let value = preview(
                        key.name,
                        live_limit.or(dedicated
                            .as_ref()
                            .or_else(|| values.as_ref().and_then(|v| v.get(key.name)))),
                    );
                    let source = if live_limit.is_some() {
                        "captured next-delegation limits".into()
                    } else if dedicated.is_some() {
                        "dedicated CLI".into()
                    } else {
                        cfg.provenance
                            .sources
                            .get(key.name)
                            .map(|sources| {
                                sources
                                    .iter()
                                    .map(|source| {
                                        let file = match source {
                                            crate::config::ConfigLayer::Global => {
                                                cfg.provenance.global_file.as_ref()
                                            }
                                            crate::config::ConfigLayer::Project => {
                                                cfg.provenance.project_file.as_ref()
                                            }
                                            crate::config::ConfigLayer::Local => {
                                                cfg.provenance.local_file.as_ref()
                                            }
                                            crate::config::ConfigLayer::Cli => None,
                                        };
                                        match file {
                                            Some(file) => format!(
                                                "{}:{}",
                                                source.label(),
                                                crate::ui::events::sanitize_output(
                                                    &file.display().to_string()
                                                )
                                            ),
                                            None => source.label().into(),
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" + ")
                            })
                            .unwrap_or_else(|| "default".into())
                    };
                    (
                        key.name.into(),
                        format!(
                            "{value} | {} | {source} | {}{} | {}",
                            kind(key.shape),
                            if key.restart_required {
                                "restart"
                            } else if key.name == "reasoning_display" {
                                "live display"
                            } else {
                                "next delegation"
                            },
                            if crate::config::editor::may_need_project_approval(key.name) {
                                ", project approval"
                            } else {
                                ""
                            },
                            key.description
                        ),
                    )
                })
                .collect();
            this.list.set_described_items(entries);
            this.list.activate();
        }
        this
    }
    fn choose(&mut self, name: &str) {
        let Some(key) = keys::lookup(name) else {
            return;
        };
        self.prefix = format!("{}{name} ", self.target.prefix());
        self.keys = false;
        let values: Vec<String> = match key.shape {
            ConfigShape::Bool => vec!["true".into(), "false".into()],
            ConfigShape::Enum(values) => {
                values.iter().map(|value| format!("\"{value}\"")).collect()
            }
            _ => Vec::new(),
        };
        self.list.set_described_items(
            values
                .into_iter()
                .map(|value| {
                    (
                        value,
                        "Enter selects; Enter again validates and saves".into(),
                    )
                })
                .collect(),
        );
        self.list.activate();
        if self.list.matches.is_empty() {
            self.list.deactivate();
        }
    }
    pub(crate) fn draft(&self) -> &str {
        &self.prefix
    }
    pub(crate) fn handle(
        &mut self,
        buffer: &mut CompactString,
        cursor: &mut usize,
        key: KeyEvent,
    ) -> bool {
        if matches!(key.code, KeyCode::Enter | KeyCode::Tab)
            && let Some(selected) = self.list.selected_name().map(str::to_owned)
        {
            if self.keys {
                self.choose(&selected);
                *buffer = self.prefix.clone().into();
            } else {
                *buffer = format!("{}{}", self.prefix, selected).into();
                self.list.deactivate();
            }
            *cursor = buffer.len();
            return true;
        }
        handlers::handle_prefixed_key(buffer, cursor, &mut self.list, &self.prefix, key)
    }
}
fn dedicated_value(cli: &crate::cli::Cli, name: &str) -> Option<serde_json::Value> {
    use serde_json::json;
    match name {
        "model" => cli.model.as_ref().map(|value| json!(value)),
        "provider" => cli.provider.as_ref().map(|value| json!(value)),
        "max_tokens" => cli.max_tokens.map(|value| json!(value)),
        "max_budget_usd" => cli.max_budget_usd.map(|value| json!(value)),
        "temperature" => cli.temperature.map(|value| json!(value)),
        "max_agent_turns" => cli.max_agent_turns.map(|value| json!(value)),
        "no_tools" if cli.no_tools => Some(json!(true)),
        "no_context_files" if cli.no_context_files => Some(json!(true)),
        "sandbox" if cli.no_sandbox => Some(json!(false)),
        "sandbox-backend" => cli.sandbox_backend.as_ref().map(|value| json!(value)),
        "shell" => cli.shell.as_ref().map(|value| json!(value)),
        "restrictive" if cli.restrictive => Some(json!(true)),
        "accept_all" if cli.accept_all => Some(json!(true)),
        "yolo" if cli.yolo => Some(json!(true)),
        "default_permission_mode" if cli.read_only => Some(json!("readonly")),
        "default_permission_mode" if cli.guarded => Some(json!("guarded")),
        _ => None,
    }
}
fn preview(name: &str, value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => "unset".into(),
        Some(serde_json::Value::Bool(value)) => value.to_string(),
        Some(serde_json::Value::Number(value)) => value.to_string(),
        Some(serde_json::Value::String(value))
            if matches!(
                name,
                "model" | "provider" | "theme" | "reasoning_display" | "cache_ttl" | "edit_system"
            ) =>
        {
            format!(
                "{:?}",
                crate::ui::events::sanitize_output(value)
                    .chars()
                    .take(80)
                    .collect::<String>()
            )
        }
        Some(serde_json::Value::String(_)) => "<set; hidden>".into(),
        Some(serde_json::Value::Array(values)) => format!("<{} items; hidden>", values.len()),
        Some(serde_json::Value::Object(values)) => format!("<{} entries; hidden>", values.len()),
    }
}
fn kind(shape: ConfigShape) -> &'static str {
    match shape {
        ConfigShape::Any => "TOML",
        ConfigShape::Bool => "boolean",
        ConfigShape::Integer => "integer",
        ConfigShape::Number => "number",
        ConfigShape::String => "string",
        ConfigShape::Strings => "string array",
        ConfigShape::Enum(_) => "enum",
        ConfigShape::BoolOrInteger => "bool/integer",
        _ => "table",
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config68_picker_selects_bool_enum_and_inline_without_saving() {
        let cfg = Config::default();
        let mut p = ConfigPicker::new(&cfg, &Default::default(), Target::Local, Some("sandbox"));
        let mut b = p.draft().into();
        let mut c = 0;
        assert!(p.handle(&mut b, &mut c, KeyCode::Enter.into()));
        assert_eq!(b, "/config --local sandbox true");
        assert!(!p.list.active);
        let p = ConfigPicker::new(&cfg, &Default::default(), Target::Global, Some("model"));
        assert!(!p.list.active);
        assert_eq!(p.draft(), "/config model ");
        let p = ConfigPicker::new(&cfg, &Default::default(), Target::Global, None);
        assert!(p.list.active);
        assert!(
            p.list
                .display_rows()
                .iter()
                .any(|row| row.contains("max_tokens") && row.contains("integer"))
        );
    }
    #[test]
    fn config68_dedicated_cli_values_are_visible_without_becoming_saved_defaults() {
        let cfg = Config {
            max_tokens: Some(51),
            ..Default::default()
        };
        let cli = crate::cli::Cli {
            max_tokens: Some(999),
            ..Default::default()
        };
        let picker = ConfigPicker::new(&cfg, &cli, Target::Global, None);
        let row = picker
            .list
            .display_rows()
            .into_iter()
            .find(|row| row.starts_with("max_tokens "))
            .unwrap();
        assert!(row.contains("999") && row.contains("dedicated CLI"));
        assert_eq!(cfg.max_tokens, Some(51));
        for key in keys::CONFIG_KEYS {
            assert!(!key.description.is_empty());
            let next_delegation = matches!(
                key.name,
                "subagent_context_window"
                    | "subagent_max_tokens"
                    | "task_max_report_bytes"
                    | "task_report_repair_max_tokens"
                    | "task_report_repair_max_cost_units"
                    | "task_max_turns"
                    | "task_max_prompts"
                    | "task_max_concurrency"
                    | "task_max_output_bytes"
                    | "task_max_cost_units"
                    | "task_timeout_secs"
            );
            assert_eq!(
                key.restart_required,
                key.name != "reasoning_display" && !next_delegation
            );
        }
        assert!(super::super::list::reserved_command("config"));
    }
    #[cfg(feature = "subagents")]
    #[test]
    fn subagent127_picker_exposes_every_next_delegation_limit_without_saving() {
        for key in crate::extras::subagents::LIVE_SETTING_KEYS {
            let mut picker = ConfigPicker::new(
                &Config::default(),
                &Default::default(),
                Target::Global,
                None,
            );
            // The ordinary catalogue deliberately displays only 50 matches.
            // Search each key through the actual list input path.
            for character in key.chars() {
                picker.list.char_input(character);
            }
            let rows = picker.list.display_rows();
            assert!(
                rows.iter()
                    .any(|row| row.starts_with(&format!("{key} "))
                        && row.contains("next delegation")),
                "missing {key}"
            );
        }
    }

    #[test]
    fn config68_preview_hides_credentials_and_terminal_controls() {
        assert!(
            !preview(
                "api_keys",
                Some(&serde_json::json!({"token-secret":"value-secret"}))
            )
            .contains("secret")
        );
        assert!(!preview("model", Some(&serde_json::json!("a\u{1b}[31m"))).contains('\u{1b}'));
    }
}
