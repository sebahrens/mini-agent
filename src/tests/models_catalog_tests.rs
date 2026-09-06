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
        ("anthropic", "claude-sonnet-5"),
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
    for id in ["claude-fable-5-1", "claude-opus-5", "claude-sonnet-5"] {
        assert!(
            anthropic.iter().any(|candidate| candidate == id),
            "missing {id}"
        );
    }
}

#[test]
fn unbaked_provider_has_no_catalog() {
    // ollama resolves live (local), so it is intentionally not baked.
    assert!(catalog_entries("ollama").is_none());
}
