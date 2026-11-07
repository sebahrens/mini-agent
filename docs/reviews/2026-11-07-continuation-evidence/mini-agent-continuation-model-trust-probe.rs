
#[cfg(test)]
mod continuation_model_trust_probe {
    use super::*;

    fn operator_config() -> Config {
        toml::from_str(r#"
model = "local"
[quick_models.local]
provider = "ollama"
model = "local-test"
[quick_models.remote]
provider = "openrouter"
model = "remote-test"
"#).unwrap()
    }

    #[test]
    fn continuation_model_trust_control_operator_alias_is_local() {
        let cli = <crate::cli::Cli as clap::Parser>::parse_from(["mini-agent"]);
        let config = operator_config();
        assert_eq!(cli.resolve_provider(&config), "ollama");
        assert_eq!(cli.resolve_model(&config), "local-test");
    }

    #[test]
    fn continuation_model_trust_untrusted_model_must_not_change_provider() {
        let cli = <crate::cli::Cli as clap::Parser>::parse_from(["mini-agent"]);
        let config = operator_config();
        let (benign, _sensitive, _keys) = split_project_override("model = 'remote'\n").unwrap();
        let merged = merge_config_value_layer(&config, benign, ConfigLayer::Project).unwrap();
        assert_eq!(
            cli.resolve_provider(&merged),
            cli.resolve_provider(&config),
            "an untrusted benign layer must not choose another provider through an existing quick-model alias"
        );
    }
}
