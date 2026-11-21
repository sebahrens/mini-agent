//! `--mcp-config <FILE|JSON>` and `--strict-mcp-config` (mini-agent-nvnkx.142).
//!
//! A definition handed to one invocation is operator-supplied: it is turned
//! into the same `mcp_servers` override a `--config` literal produces, so it
//! needs no project trust prompt, wins over the file layers, carries the
//! `CLI --config` origin, and still passes through the managed policy that
//! `apply_overrides` runs last. The JSON shape is the project `.mcp.json`
//! one (`{"mcpServers": {...}}`) and reuses its parser.

use std::collections::BTreeSet;

const MAX_MCP_CONFIG_ARGS: usize = 16;

/// Parse each `--mcp-config` value (a path, or inline JSON when it starts with
/// `{`) into config override tables, plus the server names it defines.
pub(crate) fn overrides_from_args(
    args: &[String],
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<(Vec<toml::Value>, BTreeSet<String>), String> {
    if args.len() > MAX_MCP_CONFIG_ARGS {
        return Err(format!(
            "--mcp-config: at most {MAX_MCP_CONFIG_ARGS} values are accepted"
        ));
    }
    let mut overrides = Vec::new();
    let mut names = BTreeSet::new();
    for (index, arg) in args.iter().enumerate() {
        let label = format!("--mcp-config #{}", index + 1);
        let content = if arg.trim_start().starts_with('{') {
            arg.clone()
        } else {
            read_bounded(std::path::Path::new(arg)).map_err(|error| format!("{label}: {error}"))?
        };
        let definitions =
            super::project::definitions(&content).map_err(|error| format!("{label}: {error}"))?;
        for (name, value) in definitions {
            let server = super::project::resolve_definition(value, lookup)
                .map_err(|error| format!("{label}: server '{name}': {error}"))?;
            let server =
                toml::Value::try_from(&server).map_err(|_| format!("{label}: server '{name}'"))?;
            let mut servers = toml::Table::new();
            servers.insert(name.clone(), server);
            let mut root = toml::Table::new();
            root.insert("mcp_servers".into(), toml::Value::Table(servers));
            overrides.push(toml::Value::Table(root));
            names.insert(name);
        }
    }
    Ok((overrides, names))
}

fn read_bounded(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let limit = (1024 * 1024) as u64;
    let mut content = String::new();
    file.take(limit + 1)
        .read_to_string(&mut content)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if content.len() as u64 > limit {
        return Err(format!("{}: exceeds 1 MiB", path.display()));
    }
    Ok(content)
}

/// `--strict-mcp-config`: keep only the servers the flags defined.
pub(crate) fn retain_only(cfg: &mut crate::config::Config, keep: &BTreeSet<String>) {
    if let Some(servers) = cfg.mcp_servers.as_mut() {
        servers.retain(|name, _| keep.contains(name));
    }
    cfg.mcp_origins.retain(|name, _| keep.contains(name));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn inline_json_becomes_an_mcp_servers_override() {
        let (overrides, names) = overrides_from_args(
            &[r#"{"mcpServers":{"stub":{"command":"echo","args":["hi"]}}}"#.into()],
            &none,
        )
        .unwrap();
        assert_eq!(names.into_iter().collect::<Vec<_>>(), ["stub"]);
        let servers = overrides[0]["mcp_servers"].as_table().unwrap();
        assert_eq!(servers["stub"]["command"].as_str(), Some("echo"));
    }

    #[test]
    fn file_input_is_read_and_env_placeholders_expand() {
        let path =
            std::env::temp_dir().join(format!("mcp-config-test-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"mcpServers":{"remote":{"url":"https://example.test/${HOST_PART}/mcp"}}}"#,
        )
        .unwrap();
        let lookup = |name: &str| (name == "HOST_PART").then(|| "x".to_string());
        let result = overrides_from_args(&[path.display().to_string()], &lookup);
        let _ = std::fs::remove_file(&path);
        let (overrides, _) = result.unwrap();
        assert_eq!(
            overrides[0]["mcp_servers"]["remote"]["url"].as_str(),
            Some("https://example.test/x/mcp")
        );
    }

    #[test]
    fn malformed_definitions_name_the_argument() {
        let error = overrides_from_args(&[r#"{"mcpServers":{"a":{}}}"#.into()], &none).unwrap_err();
        assert!(error.contains("--mcp-config #1"), "{error}");
        let error = overrides_from_args(&["/definitely/not/here.json".into()], &none).unwrap_err();
        assert!(error.contains("--mcp-config #1"), "{error}");
    }

    #[test]
    fn strict_mode_keeps_only_flag_servers() {
        let mut cfg: crate::config::Config =
            toml::from_str("[mcp_servers.keep]\ncommand='a'\n[mcp_servers.drop]\ncommand='b'\n")
                .unwrap();
        retain_only(&mut cfg, &BTreeSet::from(["keep".to_string()]));
        let servers = cfg.mcp_servers.unwrap();
        assert!(servers.contains_key("keep") && !servers.contains_key("drop"));
    }
}
