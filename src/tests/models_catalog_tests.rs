use crate::models_catalog::catalog_entries;

fn ids(provider: &str) -> Vec<String> {
    catalog_entries(provider)
        .unwrap_or(&[])
        .iter()
        .map(|m| m.id.clone())
        .collect()
}

#[test]
fn catalog_parses_and_has_expected_providers() {
    for p in ["anthropic", "openai", "gemini", "openrouter"] {
        assert!(
            !ids(p).is_empty(),
            "missing or empty baked catalog for: {p}"
        );
    }
}

#[test]
fn openrouter_includes_default_model() {
    // The default model (deepseek-v4-pro on openrouter) must be discoverable
    // offline so the picker is useful on a fresh, network-blocked start.
    assert!(
        ids("openrouter").contains(&"deepseek/deepseek-v4-pro".to_string()),
        "default model missing from baked openrouter catalog"
    );
}

#[test]
fn direct_provider_catalog_entries_are_priced() {
    for provider in ["anthropic", "openai", "gemini"] {
        for model in catalog_entries(provider).unwrap() {
            assert!(
                model.input_price.is_some_and(|price| price > 0.0),
                "{provider}/{} has no positive input price",
                model.id
            );
            assert!(
                model.output_price.is_some_and(|price| price > 0.0),
                "{provider}/{} has no positive output price",
                model.id
            );
        }
    }
}

#[test]
fn current_direct_provider_defaults_are_catalogued_and_priced() {
    let cfg = crate::config::Config::default();
    for (provider, expected) in [
        ("anthropic", "claude-sonnet-5-5"),
        ("openai", "gpt-5.5"),
        ("gemini", "gemini-3.7-flash"),
        ("google", "gemini-3.7-flash"),
    ] {
        let (model, _) = crate::provider::default_model_for_provider(provider, &cfg).unwrap();
        assert_eq!(model, expected);
        let catalog_provider = if provider == "google" {
            "gemini"
        } else {
            provider
        };
        assert!(
            crate::config::Config::catalog_input_output_cost(catalog_provider, &model).is_some(),
            "default {provider}/{model} is missing catalog pricing"
        );
    }
}

#[test]
fn refreshed_catalog_contains_current_anthropic_generation() {
    let anthropic = ids("anthropic");
    for id in ["claude-haiku-5-5", "claude-opus-5-5", "claude-sonnet-5-5"] {
        assert!(
            anthropic.iter().any(|candidate| candidate == id),
            "missing {id}"
        );
    }
}

#[test]
fn current_models_resolve_context_and_pricing_offline() {
    for (provider, model, context) in [
        ("anthropic", "claude-haiku-5-5", 1_000_000),
        ("anthropic", "claude-opus-5-5", 1_000_000),
        ("anthropic", "claude-sonnet-5-5", 1_000_000),
        ("openai", "gpt-6-luna", 1_050_000),
        ("openai", "gpt-6-sol", 1_050_000),
        ("openai", "gpt-6.1-sol", 1_050_000),
    ] {
        let entry = catalog_entries(provider)
            .unwrap()
            .iter()
            .find(|entry| entry.id == model)
            .unwrap_or_else(|| panic!("missing {provider}/{model}"));
        assert_eq!(entry.context_length, Some(context));
        let cfg = crate::config::Config::default();
        let quick_models = crate::config::quick_models_map(&cfg);
        assert_eq!(
            cfg.known_context_window(provider, model, &quick_models),
            Some(u64::from(context))
        );
        assert_eq!(
            cfg.resolve_context_window(provider, model, &quick_models),
            u64::from(context)
        );
        assert!(crate::config::Config::catalog_input_output_cost(provider, model).is_some());
    }
}

#[test]
fn unbaked_provider_has_no_catalog() {
    // ollama resolves live (local), so it is intentionally not baked.
    assert!(catalog_entries("ollama").is_none());
}
