use std::future::Future;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::event::AgentEvent;
use crate::provider::AnyClient;

pub(crate) mod background;
pub(crate) mod builder;
pub(crate) mod prompt;
pub(crate) mod task_tool;

#[derive(Clone)]
pub(crate) struct SubagentConfig {
    pub client: AnyClient,
    /// User-facing built-in or custom-provider alias corresponding to `client`.
    pub provider_name: String,
    pub model_name: String,
    /// Retained so the TUI can restate a startup fallback after taking over
    /// stderr, and model queries can report the actual selection honestly.
    pub fallback_notice: Option<String>,
    /// CLI key retained only so a persona quick-model can select another
    /// provider with the same explicit credential semantics as startup.
    pub api_key: Option<String>,
    pub max_turns: usize,
    pub config: crate::config::Config,
}

impl SubagentConfig {
    pub(crate) fn model_status(&self) -> String {
        let current = format!(
            "current subagent model: {} / {}",
            self.provider_name, self.model_name
        );
        match &self.fallback_notice {
            Some(notice) => format!("{current}; {notice}"),
            None => current,
        }
    }

    fn select_model(&mut self, model_name: String) {
        self.model_name = model_name;
        self.fallback_notice = None;
    }
}

static CONFIG: Mutex<Option<SubagentConfig>> = Mutex::new(None);

// Scoped fixture configuration leaves the real TaskTool call, persona resolver,
// child builder, SDK dispatch, and accounting untouched, without mutating the
// production process-global config during concurrent tests.
#[cfg(test)]
tokio::task_local! { static TEST_CONFIG: SubagentConfig; }
#[cfg(test)]
pub(crate) async fn scope_test_config<F: Future>(config: SubagentConfig, work: F) -> F::Output {
    TEST_CONFIG.scope(config, work).await
}

tokio::task_local! {
    static SUBAGENT_EVENT_TX: mpsc::Sender<AgentEvent>;
}

#[derive(Debug, thiserror::Error)]
#[error("subagents: SubagentConfig not initialized (call install() at startup)")]
pub(crate) struct ConfigNotInitialized;

pub(crate) async fn scope_subagent_event_tx<F>(tx: mpsc::Sender<AgentEvent>, future: F) -> F::Output
where
    F: Future,
{
    SUBAGENT_EVENT_TX.scope(tx, future).await
}

pub(crate) fn clone_subagent_event_tx() -> Option<mpsc::Sender<AgentEvent>> {
    SUBAGENT_EVENT_TX.try_with(|tx| tx.clone()).ok()
}

pub(crate) fn with_config<F, R>(f: F) -> Result<R, ConfigNotInitialized>
where
    F: FnOnce(&SubagentConfig) -> R,
{
    #[cfg(test)]
    if let Ok(config) = TEST_CONFIG.try_with(Clone::clone) {
        return with_config_value(Some(&config), f);
    }
    let guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    with_config_value(guard.as_ref(), f)
}

fn with_config_value<F, R>(config: Option<&SubagentConfig>, f: F) -> Result<R, ConfigNotInitialized>
where
    F: FnOnce(&SubagentConfig) -> R,
{
    config.map(f).ok_or(ConfigNotInitialized)
}

/// Resolve the subagent provider/model/client exactly as interactive startup
/// does: `subagent_model` (quick-model alias or raw id with
/// `subagent_provider`) > `subagent_provider` + main model > main model. A
/// different provider gets its own client with the same explicit credential
/// semantics; if that client cannot be built the main provider is used.
pub(crate) fn resolve_config(
    cfg: &crate::config::Config,
    api_key: Option<&str>,
    main_provider: &str,
    main_model: &str,
    main_client: &AnyClient,
) -> SubagentConfig {
    let task_max_turns = cfg.task_max_turns.unwrap_or(20);
    let qm = crate::config::quick_models_map(cfg);

    let (mut sub_provider, mut sub_model) = if let Some(sa_model) = &cfg.subagent_model {
        if let Some(q) = qm.get(sa_model.as_str()) {
            (q.provider.to_string(), q.model.to_string())
        } else {
            let prov = cfg
                .subagent_provider
                .clone()
                .map(|p| p.to_string())
                .unwrap_or_else(|| main_provider.to_owned());
            (prov, sa_model.to_string())
        }
    } else if let Some(sa_prov) = &cfg.subagent_provider {
        (sa_prov.to_string(), main_model.to_owned())
    } else {
        (main_provider.to_owned(), main_model.to_owned())
    };

    let mut fallback_notice = None;
    let sub_client = if sub_provider == main_provider {
        main_client.clone()
    } else {
        match crate::provider::create_client(
            &sub_provider,
            api_key,
            &cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        ) {
            Ok(c) => c,
            Err(e) => {
                fallback_notice = Some(format!(
                    "subagent configured: {} / {} (unavailable: {}); using main model: {} / {}",
                    sub_provider, sub_model, e, main_provider, main_model
                ));
                tracing::warn!(
                    "Could not initialize subagent provider '{}' ({}); \
                     falling back to main provider '{}'. \
                     Set `subagent_provider`/`subagent_model` in config, or the \
                     provider's API key, to silence this.",
                    sub_provider,
                    e,
                    main_provider
                );
                sub_provider = main_provider.to_owned();
                sub_model = main_model.to_owned();
                main_client.clone()
            }
        }
    };

    SubagentConfig {
        client: sub_client,
        provider_name: sub_provider,
        model_name: sub_model,
        fallback_notice,
        api_key: api_key.map(str::to_owned),
        max_turns: task_max_turns,
        config: cfg.clone(),
    }
}

/// Only resource sizing can be updated without rebuilding provider or policy
/// authority. Each delegation already clones this configuration at admission.
pub(crate) const LIVE_SETTING_KEYS: &[&str] = &[
    "subagent_context_window",
    "subagent_max_tokens",
    "task_max_report_bytes",
    "task_report_repair_max_tokens",
    "task_report_repair_max_cost_units",
    "task_max_turns",
    "task_max_prompts",
    "task_max_concurrency",
    "task_max_output_bytes",
    "task_max_cost_units",
    "task_timeout_secs",
];

fn apply_limit_setting(
    config: &mut SubagentConfig,
    key: &str,
    literal: &str,
) -> Result<(), &'static str> {
    let value: toml::Table = toml::from_str(&format!("value={literal}"))
        .map_err(|_| "Invalid subagent setting (value omitted)")?;
    let value = value
        .get("value")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or("Subagent setting must be a nonnegative integer")?;
    let mut next = config.config.clone();
    macro_rules! assign {
        ($($name:ident),* $(,)?) => {
            match key {
                $(stringify!($name) => next.$name = Some(value.try_into()
                    .map_err(|_| "Subagent setting exceeds this platform's limit")?),)*
                _ => return Err("Setting requires restart"),
            }
        };
    }
    assign!(
        subagent_context_window,
        subagent_max_tokens,
        task_max_report_bytes,
        task_report_repair_max_tokens,
        task_report_repair_max_cost_units,
        task_max_turns,
        task_max_prompts,
        task_max_concurrency,
        task_max_output_bytes,
        task_max_cost_units,
        task_timeout_secs
    );
    next.validate_subagent_settings()
        .map_err(|_| "Invalid subagent limit (value omitted)")?;
    config.max_turns = next.task_max_turns.unwrap_or(20);
    config.config = next;
    Ok(())
}

fn apply_limit_setting_at(
    config: &mut SubagentConfig,
    key: &str,
    literal: &str,
    layer: crate::config::ConfigLayer,
) -> Result<bool, &'static str> {
    fn rank(layer: &crate::config::ConfigLayer) -> u8 {
        match layer {
            crate::config::ConfigLayer::Global => 0,
            crate::config::ConfigLayer::Project => 1,
            crate::config::ConfigLayer::Local => 2,
            crate::config::ConfigLayer::Cli => 3,
        }
    }
    if config
        .config
        .provenance
        .sources
        .get(key)
        .is_some_and(|sources| sources.iter().any(|source| rank(source) > rank(&layer)))
    {
        return Ok(false);
    }
    let key = LIVE_SETTING_KEYS
        .iter()
        .copied()
        .find(|known| *known == key)
        .ok_or("Setting requires restart")?;
    apply_limit_setting(config, key, literal)?;
    config
        .config
        .provenance
        .sources
        .entry(key)
        .or_default()
        .insert(layer);
    Ok(true)
}

pub(crate) fn apply_saved_limit(
    key: &str,
    literal: &str,
    layer: crate::config::ConfigLayer,
) -> Result<bool, &'static str> {
    let mut guard = CONFIG.lock().unwrap_or_else(|error| error.into_inner());
    let Some(config) = guard.as_mut() else {
        return Ok(false);
    };
    apply_limit_setting_at(config, key, literal, layer)
}

/// Presentation-only effective limit overlay. Never contains provider keys,
/// credentials, tool grants, or a mutable reference to an active child.
pub(crate) fn live_limit_values() -> Option<serde_json::Map<String, serde_json::Value>> {
    let guard = CONFIG.lock().unwrap_or_else(|error| error.into_inner());
    let config = guard.as_ref()?;
    let mut values = serde_json::to_value(&config.config)
        .ok()?
        .as_object()?
        .clone();
    values.retain(|key, _| LIVE_SETTING_KEYS.contains(&key.as_str()));
    for (key, value) in [
        (
            "subagent_context_window",
            config.config.subagent_context_window.unwrap_or(262_144),
        ),
        (
            "subagent_max_tokens",
            config.config.resolve_subagent_max_tokens(),
        ),
        (
            "task_max_report_bytes",
            config.config.resolve_task_max_report_bytes() as u64,
        ),
        (
            "task_max_output_bytes",
            config.config.resolve_task_max_output_bytes() as u64,
        ),
        (
            "task_report_repair_max_tokens",
            config.config.resolve_task_report_repair_max_tokens(),
        ),
        (
            "task_report_repair_max_cost_units",
            config.config.resolve_task_report_repair_max_cost_units(),
        ),
        ("task_max_turns", config.max_turns as u64),
        (
            "task_max_prompts",
            config.config.resolve_task_max_prompts() as u64,
        ),
        (
            "task_max_concurrency",
            config.config.resolve_task_max_concurrency() as u64,
        ),
        (
            "task_max_cost_units",
            config.config.resolve_task_max_cost_units(),
        ),
        (
            "task_timeout_secs",
            config.config.resolve_task_timeout_secs(),
        ),
    ] {
        values.insert(key.into(), value.into());
    }
    Some(values)
}

/// Install a resolved configuration as the process-wide subagent config.
pub(crate) fn install(config: SubagentConfig) {
    if let Some(notice) = &config.fallback_notice {
        eprintln!("notice: {}", crate::ui::events::sanitize_output(notice));
    }
    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(config);
}

pub fn set_client_and_model(client: AnyClient, provider_name: String, model_name: String) {
    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cfg) = guard.as_mut() {
        cfg.client = client;
        cfg.provider_name = provider_name;
        cfg.select_model(model_name);
    }
}

pub fn set_model_name(model_name: String) {
    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cfg) = guard.as_mut() {
        cfg.select_model(model_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_runtime_model_selection_clears_the_fallback_notice() {
        let cfg = crate::config::Config {
            subagent_provider: Some("missing-subagent-provider".into()),
            ..Default::default()
        };
        let main = crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            &cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap();
        let mut resolved = resolve_config(
            &cfg,
            Some("unused-test-key"),
            "openrouter",
            "main-model",
            &main,
        );
        assert!(resolved.fallback_notice.is_some());
        resolved.select_model("chosen-runtime-model".into());
        assert!(resolved.fallback_notice.is_none());
        assert_eq!(
            resolved.model_status(),
            "current subagent model: openrouter / chosen-runtime-model"
        );
    }

    #[test]
    fn with_config_without_init_returns_error() {
        let result = with_config_value(None, |_| ());
        assert!(matches!(result, Err(ConfigNotInitialized)));
    }
}

#[cfg(test)]
mod settings127_tests {
    use super::*;
    #[test]
    fn subagent127_next_delegation_updates_only_limits_and_keeps_active_snapshot() {
        let cfg = crate::config::Config::default();
        let main = crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            &cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap();
        let mut installed = resolve_config(
            &cfg,
            Some("unused-test-key"),
            "openrouter",
            "test-model",
            &main,
        );
        let active_child = installed.clone();
        use crate::config::ConfigLayer;
        assert!(
            apply_limit_setting_at(
                &mut installed,
                "subagent_max_tokens",
                "32768",
                ConfigLayer::Project
            )
            .unwrap()
        );
        assert!(
            !apply_limit_setting_at(
                &mut installed,
                "subagent_max_tokens",
                "4096",
                ConfigLayer::Global
            )
            .unwrap()
        );
        apply_limit_setting(&mut installed, "task_max_turns", "31").unwrap();
        apply_limit_setting(&mut installed, "task_max_report_bytes", "524288").unwrap();
        assert_eq!(installed.config.resolve_subagent_max_tokens(), 32768);
        assert_eq!(installed.max_turns, 31);
        assert_eq!(active_child.config.resolve_subagent_max_tokens(), 16384);
        assert_eq!(active_child.max_turns, 20);
        assert_eq!(installed.provider_name, active_child.provider_name);
        assert_eq!(installed.model_name, active_child.model_name);
        assert_eq!(installed.api_key, active_child.api_key);
        assert!(apply_limit_setting(&mut installed, "subagent_model", "32768").is_err());
        let before = installed.config.resolve_subagent_max_tokens();
        assert!(apply_limit_setting(&mut installed, "subagent_max_tokens", "0").is_err());
        assert_eq!(installed.config.resolve_subagent_max_tokens(), before);
    }
}
