use std::collections::HashMap;

use crate::auth::{AuthResolver, ProviderKind};

fn mock_env(vars: Vec<(&str, &str)>) -> impl Fn(&str) -> Result<String, std::env::VarError> {
    let map: HashMap<String, String> = vars
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name: &str| map.get(name).cloned().ok_or(std::env::VarError::NotPresent)
}

// --- AuthResolver Tests ---

#[test]
fn auth_resolver_returns_cli_key_when_provided() {
    let resolver = AuthResolver::new(ProviderKind::OpenAI).with_cli_key(Some("sk-test-key-123"));
    let result = resolver.resolve().unwrap();
    assert_eq!(result, "sk-test-key-123");
}

#[test]
fn auth_resolver_returns_env_var_when_no_cli_key() {
    let env = mock_env(vec![("OPENAI_API_KEY", "env-key-456")]);
    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(None)
        .with_config_keys(None);
    let result = resolver.resolve_with_env(env).unwrap();
    assert_eq!(result, "env-key-456");
}

#[test]
fn auth_resolver_returns_config_key_when_no_env() {
    let mut keys = HashMap::new();
    keys.insert("openai".to_string(), "config-key-789".to_string());

    let env = mock_env(vec![]);
    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(None)
        .with_config_keys(Some(&keys));
    let result = resolver.resolve_with_env(env).unwrap();
    assert_eq!(result, "config-key-789");
}

#[test]
fn auth_resolver_cli_key_takes_priority_over_env() {
    let env = mock_env(vec![("OPENAI_API_KEY", "env-key")]);
    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(Some("cli-key"))
        .with_config_keys(None);
    let result = resolver.resolve_with_env(env).unwrap();
    assert_eq!(result, "cli-key");
}

#[test]
fn auth_resolver_env_takes_priority_over_config() {
    let env = mock_env(vec![("OPENAI_API_KEY", "env-key")]);
    let mut keys = HashMap::new();
    keys.insert("openai".to_string(), "config-key".to_string());

    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(None)
        .with_config_keys(Some(&keys));
    let result = resolver.resolve_with_env(env).unwrap();
    assert_eq!(result, "env-key");
}

#[test]
fn auth_resolver_falls_back_for_empty_cli_key() {
    let env = mock_env(vec![("OPENAI_API_KEY", "env-key")]);
    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(Some("")) // Empty CLI key should be ignored
        .with_config_keys(None);
    let result = resolver.resolve_with_env(env).unwrap();
    assert_eq!(result, "env-key");
}

#[test]
fn auth_resolver_falls_back_to_custom_provider_name() {
    let env = mock_env(vec![]);
    let mut keys = HashMap::new();
    keys.insert("local-vllm".to_string(), "custom-provider-key".to_string());

    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(None)
        .with_config_keys(Some(&keys))
        .with_custom_provider_name(Some("local-vllm"));
    let result = resolver.resolve_with_env(env).unwrap();
    assert_eq!(result, "custom-provider-key");
}

#[test]
fn auth_resolver_ollama_returns_empty_key() {
    let env = mock_env(vec![]);
    let resolver = AuthResolver::new(ProviderKind::Ollama)
        .with_cli_key(None)
        .with_env_override(None)
        .with_config_keys(None);
    let result = resolver.resolve_with_env(env).unwrap();
    assert!(result.is_empty());
}

#[test]
fn auth_resolver_errors_when_no_key_available() {
    let env = mock_env(vec![]);
    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(None)
        .with_config_keys(None);
    let result = resolver.resolve_with_env(env);
    assert!(result.is_err());
}

#[test]
fn provider_kind_from_name_recognizes_all() {
    assert_eq!(
        ProviderKind::from_name("openrouter"),
        Some(ProviderKind::OpenRouter)
    );
    assert_eq!(
        ProviderKind::from_name("openai"),
        Some(ProviderKind::OpenAI)
    );
    assert_eq!(
        ProviderKind::from_name("anthropic"),
        Some(ProviderKind::Anthropic)
    );
    assert_eq!(
        ProviderKind::from_name("gemini"),
        Some(ProviderKind::Gemini)
    );
    assert_eq!(
        ProviderKind::from_name("google"),
        Some(ProviderKind::Gemini)
    );
    assert_eq!(
        ProviderKind::from_name("ollama"),
        Some(ProviderKind::Ollama)
    );
    // "custom" is an alias for OpenAI
    assert_eq!(
        ProviderKind::from_name("custom"),
        Some(ProviderKind::OpenAI)
    );
}

#[test]
fn provider_kind_from_name_case_insensitive() {
    assert_eq!(
        ProviderKind::from_name("OPENAI"),
        Some(ProviderKind::OpenAI)
    );
    assert_eq!(
        ProviderKind::from_name("OpenAi"),
        Some(ProviderKind::OpenAI)
    );
}

#[test]
fn provider_kind_from_name_returns_none_for_unknown() {
    assert_eq!(ProviderKind::from_name("unknown"), None);
}

// --- Custom providers must not borrow built-in credentials (mini-agent-afj7) ---

#[test]
fn custom_provider_does_not_fall_back_to_kind_env_var() {
    // A real OpenAI key in the environment must never be sent to a
    // third-party base_url just because the custom provider is OpenAI-shaped.
    let env = mock_env(vec![("OPENAI_API_KEY", "sk-real-openai-key")]);
    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(None)
        .with_env_override(Some("VLLM_API_KEY"))
        .with_config_keys(None)
        .with_custom_provider_name(Some("local-vllm"));
    let error = resolver
        .resolve_with_env(env)
        .expect_err("custom provider must not inherit OPENAI_API_KEY");
    let message = error.to_string();
    assert!(message.contains("VLLM_API_KEY"), "{message}");
    assert!(message.contains("local-vllm"), "{message}");
}

#[test]
fn custom_provider_without_api_key_env_does_not_use_kind_env_var() {
    let env = mock_env(vec![("OPENAI_API_KEY", "sk-real-openai-key")]);
    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(None)
        .with_env_override(None)
        .with_config_keys(None)
        .with_custom_provider_name(Some("local-vllm"));
    assert!(resolver.resolve_with_env(env).is_err());
}

#[test]
fn custom_provider_does_not_fall_back_to_kind_config_key() {
    let env = mock_env(vec![]);
    let mut keys = HashMap::new();
    keys.insert("openai".to_string(), "sk-real-openai-key".to_string());
    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(None)
        .with_env_override(Some("VLLM_API_KEY"))
        .with_config_keys(Some(&keys))
        .with_custom_provider_name(Some("local-vllm"));
    assert!(resolver.resolve_with_env(env).is_err());
}

#[test]
fn custom_provider_uses_its_own_env_var_and_config_entry() {
    let env = mock_env(vec![
        ("OPENAI_API_KEY", "sk-real-openai-key"),
        ("VLLM_API_KEY", "vllm-key"),
    ]);
    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(None)
        .with_env_override(Some("VLLM_API_KEY"))
        .with_config_keys(None)
        .with_custom_provider_name(Some("local-vllm"));
    assert_eq!(resolver.resolve_with_env(env).unwrap(), "vllm-key");

    let env = mock_env(vec![("OPENAI_API_KEY", "sk-real-openai-key")]);
    let mut keys = HashMap::new();
    keys.insert("openai".to_string(), "sk-real-openai-key".to_string());
    keys.insert("local-vllm".to_string(), "vllm-config-key".to_string());
    let resolver = AuthResolver::new(ProviderKind::OpenAI)
        .with_cli_key(None)
        .with_env_override(Some("VLLM_API_KEY"))
        .with_config_keys(Some(&keys))
        .with_custom_provider_name(Some("local-vllm"));
    assert_eq!(resolver.resolve_with_env(env).unwrap(), "vllm-config-key");
}

#[test]
fn builtin_provider_named_by_slug_keeps_kind_fallbacks() {
    // provider.rs passes the provider name for built-ins too; they keep the
    // ordinary env var / api_keys[<slug>] resolution.
    let env = mock_env(vec![("OPENROUTER_API_KEY", "or-env-key")]);
    let resolver = AuthResolver::new(ProviderKind::OpenRouter)
        .with_cli_key(None)
        .with_env_override(None)
        .with_config_keys(None)
        .with_custom_provider_name(Some("openrouter"));
    assert_eq!(resolver.resolve_with_env(env).unwrap(), "or-env-key");

    let env = mock_env(vec![]);
    let mut keys = HashMap::new();
    keys.insert("gemini".to_string(), "gemini-config-key".to_string());
    let resolver = AuthResolver::new(ProviderKind::Gemini)
        .with_cli_key(None)
        .with_config_keys(Some(&keys))
        .with_custom_provider_name(Some("google"));
    assert_eq!(resolver.resolve_with_env(env).unwrap(), "gemini-config-key");
}
