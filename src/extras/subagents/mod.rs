use std::future::Future;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::event::AgentEvent;
use crate::provider::AnyClient;

pub(crate) mod background;
pub(crate) mod builder;
pub(crate) mod cost;
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
    /// The session's own provider: the only provider `api_key` may be sent to.
    pub session_provider: String,
    /// The `--api-key` value, valid for `session_provider` only. A persona
    /// quick-model on any other provider resolves that provider's own
    /// designated credential instead, so a vendor key never reaches a
    /// third-party `base_url`.
    pub api_key: Option<String>,
    pub max_turns: usize,
    /// Shared so each delegation snapshots it by reference count instead of
    /// cloning the whole configuration under the global lock; updates swap in
    /// a fresh `Arc` and leave admitted snapshots untouched.
    pub config: std::sync::Arc<crate::config::Config>,
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

/// Whether delegation is configured for runs started now (the task tool can
/// then route child events through the parent runner's event channel).
pub(crate) fn installed() -> bool {
    #[cfg(test)]
    if TEST_CONFIG.try_with(|_| ()).is_ok() {
        return true;
    }
    CONFIG.lock().unwrap_or_else(|e| e.into_inner()).is_some()
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
/// different provider gets its own client resolved from that provider's own
/// designated credential (never the `--api-key`, which is scoped to the main
/// provider); if that client cannot be built the main provider is used.
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
            None,
            cfg.custom_providers_map(),
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
        session_provider: main_provider.to_owned(),
        api_key: api_key.map(str::to_owned),
        max_turns: task_max_turns,
        config: std::sync::Arc::new(cfg.clone()),
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
    "task_max_output_bytes",
    "task_max_cost_units",
    "task_cost_weighting",
    "task_cost_weights",
    "task_cost_overrides",
    "task_timeout_secs",
];

fn apply_limit_setting(
    config: &mut SubagentConfig,
    key: &str,
    literal: &str,
) -> Result<(), &'static str> {
    // TOML inline tables and JSON objects share the same typed validation.
    // Parse into a candidate first: malformed or non-finite weights cannot
    // change installed limits, provenance, or an admitted child's snapshot.
    let invalid = "Invalid subagent setting (value omitted)";
    let value = match toml::from_str::<toml::Table>(&format!("value={literal}")) {
        Ok(mut table) if table.len() == 1 && table.contains_key("value") => {
            table.remove("value").ok_or(invalid)?
        }
        _ => {
            let json: serde_json::Value = serde_json::from_str(literal).map_err(|_| invalid)?;
            toml::Value::try_from(json).map_err(|_| invalid)?
        }
    };
    let mut next = crate::config::Config::clone(&config.config);
    macro_rules! assign_integer {
        ($($name:ident),* $(,)?) => {{
            let number = value.as_integer()
                .and_then(|number| u64::try_from(number).ok())
                .ok_or("Subagent setting must be a nonnegative integer")?;
            match key {
                $(stringify!($name) => next.$name = Some(number.try_into()
                    .map_err(|_| "Subagent setting exceeds this platform's limit")?),)*
                _ => return Err("Setting requires restart"),
            }
        }};
    }
    match key {
        "task_cost_weighting" => {
            next.task_cost_weighting = Some(value.try_into().map_err(|_| invalid)?)
        }
        "task_cost_weights" => {
            next.task_cost_weights = Some(value.try_into().map_err(|_| invalid)?)
        }
        "task_cost_overrides" => {
            next.task_cost_overrides = Some(value.try_into().map_err(|_| invalid)?)
        }
        _ => assign_integer!(
            subagent_context_window,
            subagent_max_tokens,
            task_max_report_bytes,
            task_report_repair_max_tokens,
            task_report_repair_max_cost_units,
            task_max_turns,
            task_max_prompts,
            task_max_output_bytes,
            task_max_cost_units,
            task_timeout_secs
        ),
    }
    next.validate_subagent_settings()
        .map_err(|_| "Invalid subagent limit (value omitted)")?;
    config.max_turns = next.task_max_turns.unwrap_or(20);
    config.config = std::sync::Arc::new(next);
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
    std::sync::Arc::make_mut(&mut config.config)
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
    live_limit_values_for(guard.as_ref()?)
}

fn live_limit_values_for(
    config: &SubagentConfig,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut values = serde_json::to_value(&*config.config)
        .ok()?
        .as_object()?
        .clone();
    values.retain(|key, _| LIVE_SETTING_KEYS.contains(&key.as_str()));
    for (key, value) in [
        (
            "subagent_context_window",
            config.config.resolve_subagent_context_window(
                &config.provider_name,
                &config.model_name,
                config.config.subagent_model.as_deref(),
            ),
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
    values.insert(
        "task_cost_weighting".into(),
        serde_json::to_value(config.config.resolve_task_cost_weighting()).ok()?,
    );
    values.insert(
        "task_cost_weights".into(),
        serde_json::to_value(config.config.resolve_task_cost_weights()).ok()?,
    );
    values.insert(
        "task_cost_overrides".into(),
        serde_json::to_value(
            config
                .config
                .task_cost_overrides
                .as_ref()
                .cloned()
                .unwrap_or_default(),
        )
        .ok()?,
    );
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
            cfg.custom_providers_map(),
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
    fn cost_setting_fixture() -> SubagentConfig {
        let cfg = crate::config::Config::default();
        let client = crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            cfg.custom_providers_map(),
            None,
        )
        .unwrap();
        resolve_config(&cfg, None, "openrouter", "fixture-child", &client)
    }

    #[test]
    fn subagent1301_cost_settings_update_next_delegation_with_layer_precedence() {
        use crate::config::{ConfigLayer, TaskCostWeighting};
        let mut installed = cost_setting_fixture();
        let active_child = installed.clone();
        assert!(
            apply_limit_setting_at(
                &mut installed,
                "task_cost_weighting",
                "\"fixed\"",
                ConfigLayer::Project
            )
            .unwrap()
        );
        assert!(
            apply_limit_setting_at(
                &mut installed,
                "task_cost_weights",
                "{input=2.5, output=7.0}",
                ConfigLayer::Project
            )
            .unwrap()
        );
        // JSON objects and nested TOML tables both pass the same typed checks.
        assert!(
            apply_limit_setting_at(
                &mut installed,
                "task_cost_overrides",
                r#"{"exact-child":{"weighting":"fixed","weights":{"output":9.0}}}"#,
                ConfigLayer::Project
            )
            .unwrap()
        );
        assert!(
            !apply_limit_setting_at(
                &mut installed,
                "task_cost_weighting",
                "\"auto\"",
                ConfigLayer::Global
            )
            .unwrap()
        );
        assert!(
            !apply_limit_setting_at(
                &mut installed,
                "task_cost_weights",
                "{input=1.0}",
                ConfigLayer::Global
            )
            .unwrap()
        );
        assert!(
            !apply_limit_setting_at(
                &mut installed,
                "task_cost_overrides",
                "{}",
                ConfigLayer::Global
            )
            .unwrap()
        );
        assert_eq!(
            installed.config.resolve_task_cost_weighting(),
            TaskCostWeighting::Fixed
        );
        assert_eq!(installed.config.resolve_task_cost_weights().input, 2.5);
        assert_eq!(installed.config.resolve_task_cost_weights().output, 7.0);
        let displayed = live_limit_values_for(&installed).unwrap();
        assert_eq!(displayed["task_cost_weighting"], "fixed");
        assert_eq!(displayed["task_cost_weights"]["input"], 2.5);
        assert_eq!(
            displayed["task_cost_overrides"]["exact-child"]["weights"]["output"],
            9.0
        );
        assert!(!displayed.contains_key("api_keys"));
        assert_eq!(
            active_child.config.resolve_task_cost_weighting(),
            TaskCostWeighting::Auto
        );
        assert_eq!(active_child.config.resolve_task_cost_weights().input, 1.0);
        assert!(
            active_child
                .config
                .task_cost_overrides
                .as_ref()
                .is_none_or(|map| map.is_empty())
        );
        assert_eq!(active_child.config.resolve_task_max_cost_units(), 4_000_000);
    }

    #[test]
    fn subagent1301_invalid_cost_settings_leave_installed_snapshot_and_provenance_unchanged() {
        use crate::config::ConfigLayer;
        let mut installed = cost_setting_fixture();
        apply_limit_setting_at(
            &mut installed,
            "task_cost_overrides",
            "{ child={weighting=\"fixed\", weights={input=2.0}} }",
            ConfigLayer::Project,
        )
        .unwrap();
        let before = serde_json::to_value(&*installed.config).unwrap();
        let sources = installed.config.provenance.sources.clone();
        for (key, literal) in [
            ("task_cost_weighting", "\"unknown\""),
            ("task_cost_weights", "[]"),
            ("task_cost_weights", "{input=nan}"),
            ("task_cost_weights", "{output=inf}"),
            ("task_cost_weights", "{input=-1.0}"),
            ("task_cost_weights", "{unexpected=1.0}"),
            ("task_cost_overrides", "{ child={weights={input=nan}} }"),
            ("task_cost_overrides", "{ child={unknown=1} }"),
            ("task_cost_weights", "{input=2.0}\nother=3"),
        ] {
            assert!(
                apply_limit_setting_at(&mut installed, key, literal, ConfigLayer::Local).is_err(),
                "{key}"
            );
            assert_eq!(serde_json::to_value(&*installed.config).unwrap(), before);
            assert_eq!(installed.config.provenance.sources, sources);
        }
    }

    #[test]
    fn subagent130_live_limit_overlay_uses_selected_child_capacity() {
        let mut cfg = crate::config::Config {
            context_window: Some(1_000_000),
            subagent_model: Some("child".into()),
            ..Default::default()
        };
        cfg.quick_models = serde_json::from_value(serde_json::json!({
            "child": {"provider":"openrouter", "model":"known-child", "context_window":1048576}
        }))
        .unwrap();
        let client = crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            cfg.custom_providers_map(),
            None,
        )
        .unwrap();
        let mut installed = resolve_config(&cfg, None, "openrouter", "main-only", &client);
        assert_eq!(
            live_limit_values_for(&installed).unwrap()["subagent_context_window"],
            1_048_576
        );
        installed.select_model("unknown-child".into());
        assert_eq!(
            live_limit_values_for(&installed).unwrap()["subagent_context_window"],
            131_072
        );
        assert_eq!(
            live_limit_values_for(&installed).unwrap()["subagent_max_tokens"],
            32_000
        );
        apply_limit_setting(&mut installed, "subagent_context_window", "524288").unwrap();
        assert_eq!(
            live_limit_values_for(&installed).unwrap()["subagent_context_window"],
            524_288
        );
    }

    #[test]
    fn subagent127_next_delegation_updates_only_limits_and_keeps_active_snapshot() {
        let cfg = crate::config::Config::default();
        let main = crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            cfg.custom_providers_map(),
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
        assert_eq!(active_child.config.resolve_subagent_max_tokens(), 32_000);
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
