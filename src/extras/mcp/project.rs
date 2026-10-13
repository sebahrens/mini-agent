//! Bounded project MCP definitions. Raw bytes own trust; expanded values never persist.
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::config::McpServerConfig;

pub(crate) const MAX_PROJECT_MCP_BYTES: usize = 1024 * 1024;
const MAX_SERVERS: usize = 128;
const MAX_VALUE_BYTES: usize = 64 * 1024;
const MAX_ARGV: usize = 256;

#[derive(Debug, Clone)]
pub(crate) struct ServerOrigin {
    pub source: String,
    pub selected: bool,
    pub inactive: Option<String>,
    pub digest: Option<String>,
}

pub(crate) fn digest(content: &str) -> String {
    crate::hex::encode_lower(Sha256::digest(content.as_bytes()))
}

/// Revalidate the captured selected definition at an admission boundary. The
/// source metadata is never sufficient by itself: workspace, raw file bytes,
/// current private trust and the selected server must still agree.
/// An absent origin preserves explicitly constructed/manual config callers.
pub(crate) fn server_source_is_current(
    workspace: &crate::paths::WorkspaceBinding,
    name: &str,
    server: Option<&McpServerConfig>,
    origin: Option<&ServerOrigin>,
) -> bool {
    if workspace.validate().is_err() {
        return false;
    }
    let Some(origin) = origin else {
        return true;
    };
    if !origin.selected || origin.inactive.is_some() {
        return false;
    }
    if let Some(expected) = &origin.digest {
        let Ok(snapshot) = FileSnapshot::read(workspace, Path::new(&origin.source)) else {
            return false;
        };
        if digest(&snapshot.content) != *expected {
            return false;
        }
        let Ok(paths) = crate::paths::AppPaths::from_process(Some(workspace.root().to_path_buf()))
        else {
            return false;
        };
        if origin.source == ".mcp.json" {
            return crate::config::project_mcp_is_trusted(&paths, workspace, &snapshot.content);
        }
        return crate::config::project_config_is_trusted_in_workspace_readonly(&paths, workspace);
    }
    if origin.source == "built-in" {
        return server.is_some_and(|server| server.trusted_identity().is_some());
    }
    let path = Path::new(&origin.source);
    let (Some(parent), Some(name_in_file)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let Ok(binding) = crate::paths::WorkspaceBinding::capture(parent) else {
        return false;
    };
    let Ok(snapshot) = FileSnapshot::read_private(&binding, Path::new(name_in_file)) else {
        return false;
    };
    if !snapshot.exists() {
        return false;
    }
    let Ok(cfg) = crate::config::parse_config_content(path, &snapshot.content) else {
        return false;
    };
    cfg.mcp_servers
        .as_ref()
        .and_then(|servers| servers.get(name))
        .and_then(|server| serde_json::to_value(server).ok())
        == server.and_then(|server| serde_json::to_value(server).ok())
}

pub(crate) fn definitions(content: &str) -> Result<BTreeMap<String, Value>, &'static str> {
    let definitions = raw_definitions(content)?;
    for value in definitions.values() {
        parse_definition(value.clone())?;
    }
    Ok(definitions)
}

fn raw_definitions(content: &str) -> Result<BTreeMap<String, Value>, &'static str> {
    if content.len() > MAX_PROJECT_MCP_BYTES {
        return Err("project MCP file exceeds 1 MiB");
    }
    let value: Value =
        serde_json::from_str(content).map_err(|_| "project MCP file is not valid JSON")?;
    let object = value
        .as_object()
        .ok_or("project MCP file must be an object")?;
    let Some(servers) = object.get("mcpServers") else {
        return Ok(BTreeMap::new());
    };
    let servers = servers.as_object().ok_or("mcpServers must be an object")?;
    if servers.len() > MAX_SERVERS {
        return Err("project MCP file exceeds 128 servers");
    }
    let mut result = BTreeMap::new();
    for (name, value) in servers {
        if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
            return Err("project MCP server name is invalid");
        }
        result.insert(name.clone(), value.clone());
    }
    Ok(result)
}

pub(crate) fn parse_definition(mut value: Value) -> Result<McpServerConfig, &'static str> {
    let object = value
        .as_object_mut()
        .ok_or("MCP server must be an object")?;
    for flag in ["disabled", "enabled"] {
        if object.get(flag).is_some_and(|flag| !flag.is_boolean()) {
            return Err("MCP enabled/disabled flags must be booleans");
        }
    }
    let command = object.contains_key("command");
    let url = object.contains_key("url");
    if command == url {
        return Err("MCP server requires exactly one command or URL");
    }
    if let Some(kind) = object.get("type") {
        let valid = if command {
            kind == "stdio"
        } else {
            kind == "http" || kind == "streamable-http"
        };
        if !valid {
            return Err("MCP server transport type is unsupported");
        }
    }
    if let Some(allow) = object.remove("allowLocal")
        && object.insert("allow_local".into(), allow).is_some()
    {
        return Err("duplicate MCP local policy");
    }
    let server: McpServerConfig =
        serde_json::from_value(value).map_err(|_| "MCP server fields have invalid types")?;
    if let McpServerConfig::Command { command, args, .. } = &server
        && (command.is_empty() || command.len() > MAX_VALUE_BYTES || args.len() > MAX_ARGV)
    {
        return Err("MCP command or argument count exceeds bounds");
    }
    Ok(server)
}

/// One pass: environment text is literal data, never recursively expanded or evaluated.
fn expand(value: &str, lookup: &impl Fn(&str) -> Option<String>) -> Result<String, String> {
    if value.len() > MAX_VALUE_BYTES {
        return Err("MCP value exceeds 64 KiB".into());
    }
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        let end = rest
            .find('}')
            .ok_or("malformed MCP environment reference")?;
        let name = &rest[..end];
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
        {
            return Err("invalid MCP environment variable name".into());
        }
        let replacement =
            lookup(name).ok_or_else(|| format!("missing MCP environment variable {name}"))?;
        if replacement.contains('\0')
            || out.len().saturating_add(replacement.len()) > MAX_VALUE_BYTES
        {
            return Err("expanded MCP value exceeds bounds or contains NUL".into());
        }
        out.push_str(&replacement);
        rest = &rest[end + 1..];
    }
    if out.len().saturating_add(rest.len()) > MAX_VALUE_BYTES || rest.contains('\0') {
        return Err("expanded MCP value exceeds bounds or contains NUL".into());
    }
    out.push_str(rest);
    Ok(out)
}

pub(crate) fn resolve_definition(
    value: Value,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<McpServerConfig, String> {
    let disabled = value.get("disabled") == Some(&Value::Bool(true))
        || value.get("enabled") == Some(&Value::Bool(false));
    let mut server = parse_definition(value).map_err(str::to_owned)?;
    if disabled {
        return Err("project MCP server is disabled".into());
    }
    let mut remaining = MAX_PROJECT_MCP_BYTES;
    let mut resolve = |value: &str| -> Result<String, String> {
        let expanded = expand(value, lookup)?;
        if expanded.len() > remaining {
            return Err("expanded MCP server exceeds 1 MiB".into());
        }
        remaining -= expanded.len();
        Ok(expanded)
    };
    match &mut server {
        McpServerConfig::Command {
            command,
            args,
            cwd,
            env,
            ..
        } => {
            *command = resolve(command)?;
            for arg in args {
                *arg = resolve(arg)?;
            }
            for value in env.values_mut() {
                *value = resolve(value)?;
            }
            if let Some(path) = cwd {
                let text = path.to_str().ok_or("MCP working directory is not UTF-8")?;
                *path = resolve(text)?.into();
            }
        }
        McpServerConfig::Url { url, headers, .. } => {
            *url = resolve(url)?;
            for value in headers.values_mut() {
                *value = resolve(value)?;
            }
        }
        McpServerConfig::BuiltIn { .. } => {
            return Err("project definitions cannot create built-in trust".into());
        }
    }
    Ok(server)
}

pub(crate) fn load_in_workspace(
    cfg: &mut crate::config::Config,
    workspace: &crate::paths::WorkspaceBinding,
    trust_path: &Path,
    interactive: bool,
    confirm: &dyn Fn(&str) -> bool,
) -> Result<(), String> {
    let content = match crate::config::read_project_mcp_text(workspace) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("project MCP file is not a bounded regular workspace file".into()),
    };
    // Malformed individual launch definitions remain visible/inactive while
    // other independently valid entries can use an existing exact receipt.
    let definitions = raw_definitions(&content).map_err(str::to_owned)?;
    let digest = digest(&content);
    let mut trusted = crate::config::project_mcp_is_trusted_at(trust_path, workspace, &content);
    if !trusted && interactive && !definitions.is_empty() && confirm(&review(&content)?) {
        trusted = crate::config::approve_project_mcp_at(trust_path, workspace, &digest).is_ok();
    }
    let servers = cfg.mcp_servers.get_or_insert_with(HashMap::new);
    for (name, value) in definitions {
        let resolved = if trusted {
            resolve_definition(value, &|name| std::env::var(name).ok())
        } else {
            Err("project content is not trusted".into())
        };
        let inactive = resolved.as_ref().err().cloned();
        if let Ok(server) = resolved {
            servers.insert(name.clone(), server);
            if let Some(old) = cfg.mcp_origins.get_mut(&name) {
                for origin in old {
                    origin.selected = false;
                }
            }
        }
        cfg.mcp_origins.entry(name).or_default().push(ServerOrigin {
            source: ".mcp.json".into(),
            selected: inactive.is_none(),
            inactive,
            digest: Some(digest.clone()),
        });
    }
    Ok(())
}

/// Bounded review contains only raw launch definitions and redacts secret-bearing fields.
pub(crate) fn review(content: &str) -> Result<String, String> {
    let servers = definitions(content).map_err(str::to_owned)?;
    let mut rendered = format!(
        "Project .mcp.json requests server launch authority.\nSHA-256: {}\nReview every resulting server (environment/header values redacted):\n",
        digest(content)
    );
    for (name, value) in servers {
        let disabled = value.get("disabled").and_then(Value::as_bool) == Some(true)
            || value.get("enabled").and_then(Value::as_bool) == Some(false);
        let server = parse_definition(value).map_err(str::to_owned)?;
        rendered.push_str(&format!("  {name}:\n"));
        if disabled {
            rendered.push_str("    inactive: explicitly disabled\n");
        }
        match server {
            McpServerConfig::Command {
                command,
                args,
                cwd,
                env,
                inherit_env,
                sandbox,
                network,
            } => {
                rendered.push_str(&format!(
                    "    command: {}\n",
                    serde_json::to_string(&command).unwrap()
                ));
                let mut redact_next = false;
                for arg in args {
                    let option = arg
                        .split('=')
                        .next()
                        .unwrap()
                        .to_ascii_lowercase()
                        .replace('_', "-");
                    let option = option.strip_prefix("--");
                    let sensitive = option.is_some_and(|option| {
                        matches!(
                            option,
                            "token" | "secret" | "password" | "api-key" | "apikey" | "credential"
                        ) || ["-token", "-secret", "-password", "-credential"]
                            .iter()
                            .any(|suffix| option.ends_with(suffix))
                    });
                    let shown = if redact_next {
                        "<redacted>"
                    } else if sensitive && arg.contains('=') {
                        "<secret option redacted>"
                    } else {
                        &arg
                    };
                    rendered.push_str(&format!(
                        "    arg: {}\n",
                        serde_json::to_string(shown).unwrap()
                    ));
                    redact_next = sensitive && !arg.contains('=');
                }
                rendered.push_str(&format!(
                    "    cwd: {:?}; sandbox: {:?}; network: {:?}\n",
                    cwd, sandbox, network
                ));
                if sandbox.is_none() {
                    rendered.push_str("    trusted code: no OS sandbox requested\n");
                }
                let mut env_names: Vec<_> = env.keys().collect();
                env_names.sort();
                rendered.push_str(&format!(
                    "    env names (values redacted): {:?}; inherit_env: {:?}\n",
                    env_names, inherit_env
                ));
            }
            McpServerConfig::Url {
                url,
                allow_local,
                headers,
                oauth,
            } => {
                let shown = url.split(['?', '#']).next().unwrap_or("");
                // Credentials are rejected before connection; hide them even in raw reviews.
                let shown = if shown.contains('@') {
                    "<URL with credentials redacted; unusable>"
                } else {
                    shown
                };
                let mut names: Vec<_> = headers.keys().collect();
                names.sort();
                let oauth = oauth.as_ref().and_then(|oauth| oauth.settings());
                rendered.push_str(&format!("    url (query/fragment omitted): {}\n    allow_local: {allow_local}; OAuth: {}\n    headers (values redacted): {:?}\n", serde_json::to_string(shown).unwrap(),oauth.is_some(),names));
                if let Some(oauth) = oauth {
                    rendered.push_str(&format!("    OAuth scopes: {:?}; redirect port: {}; pre-registered client id present (value redacted): {}\n",oauth.scopes,oauth.redirect_port(),oauth.client_id.is_some()));
                }
            }
            McpServerConfig::BuiltIn { .. } => {
                return Err("project cannot create builtin identity".into());
            }
        }
        if rendered.len() > MAX_VALUE_BYTES {
            return Err("project MCP trust review exceeds bounds".into());
        }
    }
    // Refuse an incomplete review rather than silently truncating recognized
    // launch parameters. Unknown retained fields are not executed by this parser.
    if rendered.len() > MAX_VALUE_BYTES {
        return Err("project MCP trust review exceeds bounds".into());
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn whole_file_review_shows_launch_scope_and_disabled_oauth_without_secret_values() {
        let raw = serde_json::json!({"mcpServers":{
            "runner":{"command":"runner","args":["--api_key=ARG_SECRET", "--client-secret", "NEXT_SECRET", "literal"],"env":{"TOKEN":"ENV_SECRET"},"inherit_env":["PATH"],"sandbox":"seatbelt","disabled":true},
            "trusted":{"command":"trusted-runner"},
            "local":{"url":"http://localhost:3000/mcp?token=URL_SECRET","allow_local":true,"headers":{"Authorization":"HEADER_SECRET"},"oauth":{"scopes":["read"],"client_id":"CLIENT_SECRET","redirect_port":8123}},
            "off":{"url":"https://example.invalid/mcp","oauth":false}
        }}).to_string();
        let text = review(&raw).unwrap();
        for secret in [
            "ARG_SECRET",
            "NEXT_SECRET",
            "ENV_SECRET",
            "URL_SECRET",
            "HEADER_SECRET",
            "CLIENT_SECRET",
        ] {
            assert!(!text.contains(secret));
        }
        assert!(text.contains("inactive: explicitly disabled"));
        assert!(
            text.contains("sandbox: Some(\"seatbelt\")")
                && text.contains("inherit_env: [\"PATH\"]")
        );
        assert!(text.contains("trusted code: no OS sandbox requested"));
        assert!(text.contains("OAuth scopes: [\"read\"]; redirect port: 8123"));
        assert!(text.contains("OAuth: false") && text.contains("literal"));
        assert!(text.contains(&digest(&raw)));
    }
    #[test]
    fn raw_definitions_are_bounded_and_never_manufacture_builtin_trust() {
        let defs=definitions(r#"{"mcpServers":{"雪":{"command":"runner","args":["a b",""],"env":{"KEY":"${SECRET}"}},"local":{"url":"http://localhost:3000/mcp","allow_local":true}}}"#).unwrap();
        assert_eq!(defs.len(), 2);
        assert!(
            parse_definition(defs["local"].clone())
                .unwrap()
                .trusted_identity()
                .is_none()
        );
        for content in [
            r#"{"mcpServers":{"bad":{"command":"x","url":"https://example.com"}}}"#,
            r#"{"mcpServers":{"bad":{"type":"sse","url":"https://example.com"}}}"#,
        ] {
            assert!(definitions(content).is_err());
        }
        assert!(definitions(&" ".repeat(MAX_PROJECT_MCP_BYTES + 1)).is_err());
    }
    #[test]
    fn substitution_is_one_pass_bounded_and_missing_secret_errors_do_not_reveal_values() {
        let lookup = |name: &str| match name {
            "TOKEN" => Some("${OTHER}雪".into()),
            "BIG" => Some("x".repeat(MAX_VALUE_BYTES + 1)),
            _ => None,
        };
        assert_eq!(
            expand("pre${TOKEN}post", &lookup).unwrap(),
            "pre${OTHER}雪post"
        );
        assert_eq!(
            expand("${MISSING}", &lookup).unwrap_err(),
            "missing MCP environment variable MISSING"
        );
        assert!(expand("${BIG}", &lookup).is_err());
        for invalid in ["${}", "${X:-secret}", "${1BAD}", "${UNFINISHED"] {
            assert!(expand(invalid, &lookup).is_err());
        }
    }
}

/// A retained file snapshot for operator-authored structural edits. Identity and
/// content are checked at publication; neither an ambient path nor a reread wins.
pub(crate) struct FileSnapshot {
    relative: std::path::PathBuf,
    identity: Option<crate::fs::CheckedMetadata>,
    base: Option<crate::fs::ContentDigest>,
    pub content: String,
}
impl FileSnapshot {
    pub(crate) fn read(
        workspace: &crate::paths::WorkspaceBinding,
        relative: &Path,
    ) -> std::io::Result<Self> {
        workspace.validate().map_err(std::io::Error::other)?;
        let file = match workspace.open_regular_relative(relative) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    relative: relative.into(),
                    identity: None,
                    base: None,
                    content: String::new(),
                });
            }
            Err(error) => return Err(error),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if file.metadata()?.nlink() != 1 {
                return Err(std::io::Error::other(
                    "MCP configuration must not be hardlinked",
                ));
            }
        }
        #[cfg(windows)]
        if crate::fs::windows_file_link_count(&file)? != 1 {
            return Err(std::io::Error::other(
                "MCP configuration must not be hardlinked",
            ));
        }
        let identity = crate::fs::checked_file_metadata(&file)?;
        let content = match crate::paths::read_utf8_bounded_status(file, MAX_PROJECT_MCP_BYTES)? {
            crate::paths::BoundedUtf8Read::Content(content) => content,
            _ => {
                return Err(std::io::Error::other(
                    "MCP configuration is oversized or not UTF-8",
                ));
            }
        };
        let base = crate::fs::ContentDigest::of(content.as_bytes());
        let current = workspace.open_regular_relative(relative)?;
        crate::fs::ensure_same_file(
            relative,
            &identity,
            &crate::fs::checked_file_metadata(&current)?,
        )?;
        if !base.matches_file(&current)? {
            return Err(crate::fs::stale_content_error(relative));
        }
        workspace.validate().map_err(std::io::Error::other)?;
        Ok(Self {
            relative: relative.into(),
            identity: Some(identity),
            base: Some(base),
            content,
        })
    }
    pub(crate) fn exists(&self) -> bool {
        self.identity.is_some()
    }

    pub(crate) fn read_private(
        workspace: &crate::paths::WorkspaceBinding,
        relative: &Path,
    ) -> std::io::Result<Self> {
        let snapshot = Self::read(workspace, relative)?;
        if let (Some(identity), Some(base)) = (&snapshot.identity, &snapshot.base) {
            let file = workspace.open_regular_relative(relative)?;
            crate::fs::validate_private_file(&file)?;
            crate::fs::ensure_same_file(
                relative,
                identity,
                &crate::fs::checked_file_metadata(&file)?,
            )?;
            if !base.matches_file(&file)? {
                return Err(crate::fs::stale_content_error(relative));
            }
            workspace.validate().map_err(std::io::Error::other)?;
        }
        Ok(snapshot)
    }

    pub(crate) fn publish(
        &self,
        workspace: &crate::paths::WorkspaceBinding,
        content: &str,
    ) -> std::io::Result<()> {
        if content.len() > MAX_PROJECT_MCP_BYTES {
            return Err(std::io::Error::other("MCP configuration exceeds 1 MiB"));
        }
        if let (Some(identity), Some(base)) = (&self.identity, &self.base) {
            workspace.replace_relative_private_atomic_expecting(
                &self.relative,
                content.as_bytes(),
                identity,
                base,
            )?;
        } else {
            workspace.create_relative_private_atomic(&self.relative, content.as_bytes())?;
        }
        let file = workspace.open_regular_relative(&self.relative)?;
        crate::fs::validate_private_file(&file)?;
        Ok(())
    }
}

pub(crate) fn edit_json(content: &str, name: &str, value: Option<Value>) -> Result<String, String> {
    let mut document = if content.is_empty() {
        serde_json::json!({"mcpServers":{}})
    } else {
        // Removing the malformed entry itself can repair a bounded catalogue.
        // Every remaining definition still must pass the full review before
        // publication/approval; unrelated invalid launch fields never slip in.
        raw_definitions(content).map_err(str::to_owned)?;
        serde_json::from_str::<Value>(content).map_err(|_| "invalid MCP JSON")?
    };
    let object = document
        .as_object_mut()
        .ok_or("MCP JSON must be an object")?;
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("mcpServers must be an object")?;
    if let Some(value) = value {
        parse_definition(value.clone()).map_err(str::to_owned)?;
        if servers.contains_key(name) {
            return Err(
                "MCP name already exists in this config; remove it explicitly first".into(),
            );
        }
        servers.insert(name.into(), value);
    } else if servers.remove(name).is_none() {
        return Err("MCP name is absent from this config".into());
    }
    let text =
        serde_json::to_string_pretty(&document).map_err(|_| "MCP JSON serialization failed")?;
    definitions(&text).map_err(str::to_owned)?;
    Ok(text + "\n")
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    #[test]
    fn captured_server_source_preserves_manual_config_but_rejects_forged_builtin_and_changed_global()
     {
        let root = crate::tests::OwnedTestDirectory::new("mcp-captured-source-policy");
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let server = parse_definition(serde_json::json!({"command":"literal-runner"})).unwrap();
        assert!(server_source_is_current(
            &workspace,
            "owned",
            Some(&server),
            None
        ));
        let mut origin = ServerOrigin {
            source: "built-in".into(),
            selected: true,
            inactive: None,
            digest: None,
        };
        assert!(!server_source_is_current(
            &workspace,
            "owned",
            Some(&server),
            Some(&origin)
        ));
        let mut builtins = crate::config::Config {
            enable_exa_mcp: Some(true),
            enable_context7_mcp: Some(false),
            enable_grepapp_mcp: Some(false),
            ..Default::default()
        };
        crate::config::inject_mcp_defaults(&mut builtins);
        let trusted = builtins
            .mcp_servers
            .as_ref()
            .unwrap()
            .get("Exa Web Search")
            .unwrap();
        assert!(server_source_is_current(
            &workspace,
            "exa",
            Some(trusted),
            Some(&origin)
        ));
        let path = root.path().join("config.json");
        let raw =
            serde_json::json!({"mcp_servers":{"owned":{"command":"literal-runner"}}}).to_string();
        crate::fs::private_atomic_create_sync(&path, raw.as_bytes()).unwrap();
        origin.source = path.display().to_string();
        assert!(server_source_is_current(
            &workspace,
            "owned",
            Some(&server),
            Some(&origin)
        ));
        origin.selected = false;
        assert!(!server_source_is_current(
            &workspace,
            "owned",
            Some(&server),
            Some(&origin)
        ));
        origin.selected = true;
        origin.inactive = Some("not trusted".into());
        assert!(!server_source_is_current(
            &workspace,
            "owned",
            Some(&server),
            Some(&origin)
        ));
        origin.inactive = None;
        crate::fs::private_atomic_write_sync(&path, b"{\"mcp_servers\":{}}").unwrap();
        assert!(!server_source_is_current(
            &workspace,
            "owned",
            Some(&server),
            Some(&origin)
        ));
    }

    #[test]
    fn structural_json_edit_preserves_unknown_fields_and_private_publication_refuses_stale_snapshots()
     {
        let root = crate::tests::OwnedTestDirectory::new("project-mcp-publication");
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let original = r#"{"future":{"keep":[1,2]},"mcpServers":{"old":{"command":"runner","futureOption":{"keep":true}}}}"#;
        std::fs::write(root.path().join(".mcp.json"), original).unwrap();
        let snapshot = FileSnapshot::read(&workspace, Path::new(".mcp.json")).unwrap();
        let changed = edit_json(
            &snapshot.content,
            "new",
            Some(serde_json::json!({"url":"http://localhost:3000/mcp","allow_local":true})),
        )
        .unwrap();
        snapshot.publish(&workspace, &changed).unwrap();
        let current: Value =
            serde_json::from_str(&std::fs::read_to_string(root.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(current["future"]["keep"], serde_json::json!([1, 2]));
        assert_eq!(current["mcpServers"]["old"]["futureOption"]["keep"], true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(root.path().join(".mcp.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(snapshot.publish(&workspace, original).is_err());
        let new = FileSnapshot::read(&workspace, Path::new(".mcp.json")).unwrap();
        std::fs::write(root.path().join(".mcp.json"), "{\"raced\":true}").unwrap();
        assert!(new.publish(&workspace, &changed).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join(".mcp.json")).unwrap(),
            "{\"raced\":true}"
        );
        let empty = FileSnapshot::read(&workspace, Path::new("new.json")).unwrap();
        std::fs::write(root.path().join("new.json"), "concurrent").unwrap();
        assert!(empty.publish(&workspace, "{}").is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("new.json")).unwrap(),
            "concurrent"
        );
    }
    #[cfg(unix)]
    #[test]
    #[allow(unsafe_code)]
    fn project_snapshot_rejects_symlink_hardlink_and_fifo_without_changing_victim() {
        use std::os::unix::fs::symlink;
        let root = crate::tests::OwnedTestDirectory::new("project-mcp-no-follow");
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let victim = root.path().join("victim");
        std::fs::write(&victim, "PRIVATE_UNCHANGED").unwrap();
        symlink(&victim, root.path().join(".mcp.json")).unwrap();
        assert!(FileSnapshot::read(&workspace, Path::new(".mcp.json")).is_err());
        std::fs::remove_file(root.path().join(".mcp.json")).unwrap();
        std::fs::hard_link(&victim, root.path().join(".mcp.json")).unwrap();
        assert!(FileSnapshot::read(&workspace, Path::new(".mcp.json")).is_err());
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "PRIVATE_UNCHANGED"
        );
        std::fs::remove_file(root.path().join(".mcp.json")).unwrap();
        std::fs::create_dir(root.path().join(".mcp.json")).unwrap();
        assert!(FileSnapshot::read(&workspace, Path::new(".mcp.json")).is_err());
        std::fs::remove_dir(root.path().join(".mcp.json")).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let name =
            std::ffi::CString::new(root.path().join(".mcp.json").as_os_str().as_bytes()).unwrap();
        // SAFETY: name is an owned NUL-terminated fixture path; no process is created.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(FileSnapshot::read(&workspace, Path::new(".mcp.json")).is_err());
    }
}

#[cfg(test)]
mod trust_tests {
    use super::*;
    #[test]
    fn project_mcp_authority_is_exact_file_content_and_never_crosses_to_project_toml_or_changed_json()
     {
        let root = crate::tests::OwnedTestDirectory::new("project-mcp-trust");
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let trust = root.path().join("private").join("project-trust.json");
        let content = r#"{"mcpServers":{"owned":{"command":"owned-runner","args":["literal雪"],"env":{"KEY":"VALUE_MUST_BE_REDACTED"}}}}"#;
        std::fs::write(root.path().join(".mcp.json"), content).unwrap();
        let mut cfg = crate::config::Config::default();
        load_in_workspace(&mut cfg, &workspace, &trust, false, &|_| {
            panic!("headless must never prompt")
        })
        .unwrap();
        assert!(cfg.mcp_servers.as_ref().unwrap().is_empty());
        assert_eq!(
            cfg.mcp_origins["owned"][0].inactive.as_deref(),
            Some("project content is not trusted")
        );
        load_in_workspace(&mut cfg, &workspace, &trust, true, &|review| {
            assert!(review.contains("owned-runner") && review.contains("literal雪"));
            assert!(!review.contains("VALUE_MUST_BE_REDACTED"));
            true
        })
        .unwrap();
        assert!(cfg.mcp_servers.as_ref().unwrap().contains_key("owned"));
        assert!(crate::config::project_mcp_is_trusted_at(
            &trust, &workspace, content
        ));
        let changed = content.replace("owned-runner", "different-runner");
        std::fs::write(root.path().join(".mcp.json"), &changed).unwrap();
        let mut restarted = crate::config::Config::default();
        load_in_workspace(&mut restarted, &workspace, &trust, false, &|_| {
            panic!("changed content is not approved")
        })
        .unwrap();
        assert!(restarted.mcp_servers.as_ref().unwrap().is_empty());
        assert!(
            crate::config::approve_project_mcp_at(&trust, &workspace, &digest(content)).is_err()
        );
        assert!(!crate::config::project_mcp_is_trusted_at(
            &trust, &workspace, &changed
        ));
        std::fs::create_dir(root.path().join(".zerostack")).unwrap();
        let toml = root.path().join(".zerostack/config.toml");
        std::fs::write(&toml, "[mcp_servers.different]\ncommand='toml-only'\n").unwrap();
        crate::config::trust_project_config(&toml, &trust).unwrap();
        assert!(!crate::config::project_mcp_is_trusted_at(
            &trust, &workspace, &changed
        ));
        let mut declined = crate::config::Config::default();
        load_in_workspace(&mut declined, &workspace, &trust, true, &|_| false).unwrap();
        assert!(declined.mcp_servers.as_ref().unwrap().is_empty());
    }
    #[test]
    fn trusted_missing_environment_stays_inactive_and_total_substitution_is_bounded() {
        let value = serde_json::json!({"command":"runner","args":vec!["${BIG}";32]});
        let error = resolve_definition(value, &|_| Some("x".repeat(MAX_VALUE_BYTES))).unwrap_err();
        assert_eq!(error, "expanded MCP server exceeds 1 MiB");
        let value = serde_json::json!({"url":"http://localhost:3000/mcp","headers":{"Authorization":"Bearer ${MISSING_SECRET}"}});
        assert_eq!(
            resolve_definition(value, &|_| None).unwrap_err(),
            "missing MCP environment variable MISSING_SECRET"
        );
    }
    #[test]
    fn disabled_definition_never_expands_secrets_and_malformed_neighbor_remains_visible() {
        let disabled = serde_json::json!({"command":"${SECRET}", "disabled":true});
        assert_eq!(
            resolve_definition(disabled, &|_| panic!(
                "disabled entry must not read environment"
            ))
            .unwrap_err(),
            "project MCP server is disabled"
        );
        assert!(
            parse_definition(serde_json::json!({"command":"runner", "enabled":"false"})).is_err()
        );
        let root = crate::tests::OwnedTestDirectory::new("project-mcp-invalid-neighbor");
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let trust = root.path().join("private/project-trust.json");
        let original =
            r#"{"mcpServers":{"valid":{"command":"owned-runner"},"invalid":{"command":7}}}"#;
        std::fs::write(root.path().join(".mcp.json"), original).unwrap();
        // No receipt exists, so both entries are visible but neither launches.
        let mut cfg = crate::config::Config::default();
        load_in_workspace(&mut cfg, &workspace, &trust, false, &|_| {
            panic!("headless must not prompt")
        })
        .unwrap();
        assert!(cfg.mcp_servers.as_ref().unwrap().is_empty());
        assert!(cfg.mcp_origins.contains_key("invalid") && cfg.mcp_origins.contains_key("valid"));
        // An explicit review refuses this whole malformed file rather than
        // silently approving the valid neighbor's future launch authority.
        assert!(review(original).is_err());
    }
}
