use std::future::Future;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::event::AgentEvent;
use crate::provider::AnyClient;

pub(crate) mod builder;
pub(crate) mod prompt;
pub(crate) mod task_tool;

pub(crate) struct SubagentConfig {
    pub client: AnyClient,
    /// User-facing built-in or custom-provider alias corresponding to `client`.
    pub provider_name: String,
    pub model_name: String,
    /// CLI key retained only so a persona quick-model can select another
    /// provider with the same explicit credential semantics as startup.
    pub api_key: Option<String>,
    pub max_turns: usize,
    pub config: crate::config::Config,
}

static CONFIG: Mutex<Option<SubagentConfig>> = Mutex::new(None);

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
        api_key: api_key.map(str::to_owned),
        max_turns: task_max_turns,
        config: cfg.clone(),
    }
}

/// Install a resolved configuration as the process-wide subagent config.
pub(crate) fn install(config: SubagentConfig) {
    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(config);
}

pub fn set_client_and_model(client: AnyClient, provider_name: String, model_name: String) {
    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cfg) = guard.as_mut() {
        cfg.client = client;
        cfg.provider_name = provider_name;
        cfg.model_name = model_name;
    }
}

pub fn set_model_name(model_name: String) {
    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cfg) = guard.as_mut() {
        cfg.model_name = model_name;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_config_without_init_returns_error() {
        let result = with_config_value(None, |_| ());
        assert!(matches!(result, Err(ConfigNotInitialized)));
    }
}
