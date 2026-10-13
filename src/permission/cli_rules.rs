//! Invocation-only permission seeds. Values remain private because complete
//! shell scripts and URL patterns may include credentials.

use std::collections::HashMap;
use std::convert::Infallible;
use std::str::FromStr;

use super::PermissionConfig;

const MAX_CLI_RULES: usize = 256;
const MAX_CLI_RULE_BYTES: usize = 8192;

#[derive(Clone)]
pub struct CliPermissionRule(String);

impl std::fmt::Debug for CliPermissionRule {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CliPermissionRule([redacted])")
    }
}

impl FromStr for CliPermissionRule {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Validate with flag/ordinal diagnostics at the common policy boundary.
        // Clap's value-parser errors echo the supplied value, which may contain
        // a secret inside an exact shell script.
        Ok(Self(value.to_owned()))
    }
}

pub(crate) fn config(cli: &crate::cli::Cli) -> anyhow::Result<PermissionConfig> {
    if cli
        .allow
        .len()
        .saturating_add(cli.deny.len())
        .saturating_add(cli.allow_prefix.len())
        > MAX_CLI_RULES
    {
        anyhow::bail!("CLI permission seeds are limited to {MAX_CLI_RULES} rules per invocation");
    }
    fn entries(
        flag: &str,
        values: &[CliPermissionRule],
        prefix: bool,
    ) -> anyhow::Result<Option<HashMap<String, Vec<String>>>> {
        let mut result: HashMap<String, Vec<String>> = HashMap::new();
        for (index, value) in values.iter().enumerate() {
            let invalid = |reason: &str| anyhow::anyhow!("{flag} rule {}: {reason}", index + 1);
            if value.0.len() > MAX_CLI_RULE_BYTES {
                return Err(invalid("exceeds the 8192-byte rule limit"));
            }
            let (tool, pattern) = value
                .0
                .split_once(':')
                .ok_or_else(|| invalid("expected TOOL:PATTERN"))?;
            if !super::is_configurable_tool_name(tool) {
                return Err(invalid(
                    "unsupported permission tool (for example write, shell, or mcp_tool)",
                ));
            }
            if pattern.trim().is_empty() || pattern.contains('\0') {
                return Err(invalid("pattern must be nonempty and cannot contain NUL"));
            }
            if prefix {
                if !matches!(tool, "bash" | "shell") {
                    return Err(invalid("prefix grants support only bash/shell"));
                }
                super::shell_prefix::ShellPrefix::parse(pattern)
                    .map_err(|error| invalid(&error.to_string()))?;
            }
            // Canonicalize schema aliases, preserving the complete raw input.
            let tool = super::canonical_rule_tool_name(tool);
            result.entry(tool.into()).or_default().push(pattern.into());
        }
        Ok((!result.is_empty()).then_some(result))
    }
    Ok(PermissionConfig {
        allow_entries: entries("--allow", &cli.allow, false)?,
        deny_entries: entries("--deny", &cli.deny, false)?,
        allow_prefix_entries: entries("--allow-prefix", &cli.allow_prefix, true)?,
        ..PermissionConfig::default()
    })
}

/// A reporting-only clone: never pass it back to a config save or session.
/// The checker compiles the base policy first, then merges these same entries,
/// preserving built-in defaults that a new config tool key could otherwise mask.
pub(crate) fn report_config(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
) -> anyhow::Result<crate::config::Config> {
    let rules = config(cli)?;
    let mut result = cfg.clone();
    fn append(
        target: &mut Option<HashMap<String, Vec<String>>>,
        entries: Option<HashMap<String, Vec<String>>>,
    ) {
        if let Some(entries) = entries {
            let target = target.get_or_insert_with(HashMap::new);
            for (tool, patterns) in entries {
                target.entry(tool).or_default().extend(patterns);
            }
        }
    }
    append(&mut result.permission_allow, rules.allow_entries);
    append(&mut result.permission_deny, rules.deny_entries);
    append(
        &mut result.permission_allow_prefix,
        rules.allow_prefix_entries,
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_seeds_are_repeatable_and_debug_redacts_even_invalid_rule_values() {
        let cli = crate::cli::Cli::try_parse_from([
            "mini-agent",
            "--guarded",
            "--allow",
            "write:src/**",
            "--allow",
            "shell:echo PRIVATE_CANARY",
            "--deny",
            "write:**/*.env",
            "--allow-prefix",
            "bash:git status",
        ])
        .unwrap();
        assert_eq!(cli.allow.len(), 2);
        assert!(cli.guarded && !cli.yolo && !cli.accept_all && !cli.read_only);
        let rules = config(&cli).unwrap();
        assert_eq!(
            rules.allow_entries.unwrap()["shell"],
            ["echo PRIVATE_CANARY"]
        );
        assert_eq!(rules.deny_entries.unwrap()["write"], ["**/*.env"]);
        assert!(!format!("{cli:?}").contains("PRIVATE_CANARY"));
        for (flag, value) in [
            ("--allow", "PRIVATE_CANARY"),
            ("--allow", "PRIVATE_CANARY:content"),
            ("--allow", "write:"),
            ("--deny", "write:  "),
            ("--allow-prefix", "write:PRIVATE_CANARY"),
            ("--allow-prefix", "shell:echo PRIVATE_CANARY; echo another"),
        ] {
            let cli = crate::cli::Cli::try_parse_from(["mini-agent", flag, value]).unwrap();
            let error = config(&cli).unwrap_err().to_string();
            assert!(error.contains(flag) && error.contains("rule 1"), "{error}");
            assert!(!error.contains("PRIVATE_CANARY"), "{error}");
            assert!(!format!("{cli:?}").contains("PRIVATE_CANARY"));
        }
    }

    #[test]
    fn cli_seeds_validate_even_when_no_tools_or_skip_permissions_is_selected() {
        for extra in ["--no-tools", "--dangerously-skip-permissions"] {
            let cli = crate::cli::Cli::try_parse_from([
                "mini-agent",
                extra,
                "--allow-prefix",
                "shell:echo PRIVATE_CANARY; echo next",
            ])
            .unwrap();
            let authority = super::super::resolve_execution_authority(
                &cli,
                &crate::config::Config::default(),
                crate::sandbox::SandboxPolicy::Disabled,
                "disabled",
            )
            .unwrap();
            let error = super::super::build_noninteractive_permission_with_cli(
                &cli,
                &crate::config::Config::default(),
                authority,
            )
            .err()
            .expect("invalid seed must fail before bypass")
            .to_string();
            assert!(error.contains("--allow-prefix rule 1"), "{error}");
            assert!(!error.contains("PRIVATE_CANARY"), "{error}");
        }
    }

    #[test]
    fn cli_seeds_bound_rule_count_length_and_keep_reporting_overlay_separate() {
        let mut cli = crate::cli::Cli {
            allow: vec!["write:src/**".parse().unwrap()],
            ..Default::default()
        };
        let cfg = crate::config::Config {
            permission_allow: Some(HashMap::from([("read".into(), vec!["src/**".into()])])),
            ..Default::default()
        };
        let original = serde_json::to_value(&cfg).unwrap();
        let report = report_config(&cli, &cfg).unwrap();
        assert_eq!(
            report.permission_allow.as_ref().unwrap()["write"],
            ["src/**"]
        );
        assert_eq!(serde_json::to_value(&cfg).unwrap(), original);
        cli.deny = vec!["write:*".parse().unwrap(); MAX_CLI_RULES];
        assert!(config(&cli).unwrap_err().to_string().contains("256 rules"));
        cli.deny.clear();
        cli.allow = vec![
            format!("shell:{}", "x".repeat(MAX_CLI_RULE_BYTES))
                .parse()
                .unwrap(),
        ];
        assert!(config(&cli).unwrap_err().to_string().contains("8192-byte"));
        cli.allow = vec!["write:a\0b".parse().unwrap()];
        assert!(config(&cli).unwrap_err().to_string().contains("NUL"));
    }
}
