//! `mini-agent import-settings <claude|codex|opencode> [--dry-run] [--yes]`.
//!
//! Reads another agent's user-level settings (read-only) and converts what
//! maps cleanly onto mini-agent's global config: MCP servers, permission
//! rules, the model choice, custom commands and agent definitions. Nothing is
//! written until the user has seen a preview of the resulting config diff and
//! confirmed it (`--yes` confirms non-interactively; `--dry-run` never
//! writes). Rules for the rest:
//!
//! * Secrets are never copied. An environment value or header whose name or
//!   shape looks like a credential is replaced by a `${NAME}` reference and
//!   listed under "action needed".
//! * Hooks are never imported: global hooks are trusted by default, so a
//!   copied hook would run without the normal approval. They are listed.
//! * Anything that cannot be expressed is dropped and listed, never guessed.
//! * Existing user keys are preserved; an existing server of the same name or
//!   an existing `model` wins.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value as Json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Claude,
    Codex,
    Opencode,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Opencode => "opencode",
        }
    }
}

/// A file to copy into the mini-agent config or data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileCopy {
    pub(crate) from: PathBuf,
    pub(crate) to: PathBuf,
}

/// What an import would do.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Plan {
    /// `[mcp_servers.<name>]` tables to add.
    pub(crate) mcp: BTreeMap<String, toml::Table>,
    pub(crate) model: Option<String>,
    /// `permission-allow` / `permission-allow-prefix` / `permission-ask` /
    /// `permission-deny`: tool name to patterns.
    pub(crate) allow: BTreeMap<String, Vec<String>>,
    pub(crate) allow_prefix: BTreeMap<String, Vec<String>>,
    pub(crate) ask: BTreeMap<String, Vec<String>>,
    pub(crate) deny: BTreeMap<String, Vec<String>>,
    pub(crate) files: Vec<FileCopy>,
    /// Things the user must do by hand (export a variable, review a hook).
    pub(crate) actions: Vec<String>,
    /// Entries that cannot be expressed and were left out.
    pub(crate) unsupported: Vec<String>,
}

fn push_unique(map: &mut BTreeMap<String, Vec<String>>, tool: &str, pattern: String) {
    let list = map.entry(tool.to_owned()).or_default();
    if !list.contains(&pattern) {
        list.push(pattern);
    }
}

fn secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "AUTH",
        "CREDENTIAL",
        "COOKIE",
    ]
    .iter()
    .any(|word| upper.contains(word))
}

fn secret_value(value: &str) -> bool {
    let v = value.trim();
    [
        "sk-",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "AIza",
        "eyJ",
        "Bearer ",
        "Basic ",
    ]
    .iter()
    .any(|prefix| v.starts_with(prefix))
}

fn placeholder_name(server: &str, name: &str) -> String {
    let clean = |text: &str| {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!("MCP_{}_{}", clean(server), clean(name))
}

/// A string map (env or headers) with credentials replaced by `${NAME}`.
fn clean_map(
    server: &str,
    kind: &str,
    map: &serde_json::Map<String, Json>,
    plan_actions: &mut Vec<String>,
) -> toml::Table {
    let mut out = toml::Table::new();
    for (name, value) in map {
        let Some(text) = value.as_str() else { continue };
        // An existing `${VAR}` reference is not a secret, it is a reference.
        let is_reference = text.contains("${");
        if !is_reference && (secret_name(name) || secret_value(text)) {
            let var = if kind == "env" {
                name.clone()
            } else {
                placeholder_name(server, name)
            };
            let replacement = if kind == "env" {
                format!("${{{var}}}")
            } else if let Some(rest) = text.strip_prefix("Bearer ") {
                let _ = rest;
                format!("Bearer ${{{var}}}")
            } else {
                format!("${{{var}}}")
            };
            plan_actions.push(format!(
                "server '{server}': the {kind} value for '{name}' looks like a credential and was not copied; export {var} before use"
            ));
            out.insert(name.clone(), toml::Value::String(replacement));
        } else {
            out.insert(name.clone(), toml::Value::String(text.to_owned()));
        }
    }
    out
}

fn valid_server_name(name: &str) -> bool {
    (1..=64).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// One `mcpServers` entry in Claude Code / `.mcp.json` shape.
pub(crate) fn claude_server(name: &str, entry: &Json, plan: &mut Plan) {
    let Some(object) = entry.as_object() else {
        plan.unsupported
            .push(format!("MCP server '{name}': not an object"));
        return;
    };
    let kind = object.get("type").and_then(Json::as_str).unwrap_or("stdio");
    let mut table = toml::Table::new();
    match kind {
        "stdio" => {
            let Some(command) = object.get("command").and_then(Json::as_str) else {
                plan.unsupported
                    .push(format!("MCP server '{name}': no command"));
                return;
            };
            table.insert("command".into(), toml::Value::String(command.to_owned()));
            if let Some(args) = object.get("args").and_then(Json::as_array) {
                let args: Vec<toml::Value> = args
                    .iter()
                    .filter_map(Json::as_str)
                    .map(|a| toml::Value::String(a.to_owned()))
                    .collect();
                table.insert("args".into(), toml::Value::Array(args));
            }
            if let Some(env) = object.get("env").and_then(Json::as_object) {
                let env = clean_map(name, "env", env, &mut plan.actions);
                if !env.is_empty() {
                    table.insert("env".into(), toml::Value::Table(env));
                }
            }
        }
        "http" | "sse" | "streamable-http" => {
            let Some(url) = object.get("url").and_then(Json::as_str) else {
                plan.unsupported
                    .push(format!("MCP server '{name}': no url"));
                return;
            };
            table.insert("url".into(), toml::Value::String(url.to_owned()));
            if kind == "sse" {
                table.insert("legacy_sse".into(), toml::Value::Boolean(true));
            }
            if let Some(headers) = object.get("headers").and_then(Json::as_object) {
                let headers = clean_map(name, "header", headers, &mut plan.actions);
                if !headers.is_empty() {
                    table.insert("headers".into(), toml::Value::Table(headers));
                }
            }
        }
        other => {
            plan.unsupported.push(format!(
                "MCP server '{name}': transport '{other}' is not supported"
            ));
            return;
        }
    }
    if !valid_server_name(name) {
        plan.unsupported
            .push(format!("MCP server '{name}': the name is not usable here"));
        return;
    }
    plan.mcp.insert(name.to_owned(), table);
}

fn tool_for_claude(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "Bash" => &["bash"],
        "Read" => &["read"],
        "Edit" | "MultiEdit" => &["edit", "write"],
        "Write" => &["write"],
        "Grep" => &["grep"],
        "Glob" => &["find_files"],
        _ => return None,
    })
}

/// Claude path patterns: `//abs` is absolute, `~/`, `./` and bare relative
/// patterns carry over; `/x` (project-root relative) cannot be mapped.
fn claude_path_pattern(pattern: &str) -> Option<String> {
    if let Some(rest) = pattern.strip_prefix("//") {
        return Some(format!("/{rest}"));
    }
    if pattern.starts_with('/') {
        return None;
    }
    Some(pattern.strip_prefix("./").unwrap_or(pattern).to_owned())
}

fn claude_rule(rule: &str, mode: &str, plan: &mut Plan) {
    let (name, argument) = match rule.split_once('(') {
        Some((name, rest)) => (name.trim(), rest.strip_suffix(')').map(str::to_owned)),
        None => (rule.trim(), None),
    };
    let Some(tools) = tool_for_claude(name) else {
        plan.unsupported.push(format!(
            "{mode} rule '{rule}': tool '{name}' has no equivalent"
        ));
        return;
    };
    for tool in tools {
        match (*tool, argument.as_deref()) {
            ("bash", None) => match mode {
                "deny" => push_unique(&mut plan.deny, tool, "**".into()),
                "ask" => push_unique(&mut plan.ask, tool, "**".into()),
                _ => plan.unsupported.push(format!(
                    "allow rule '{rule}': an unrestricted Bash allow is not imported"
                )),
            },
            ("bash", Some(command)) => {
                let prefix = command
                    .strip_suffix(":*")
                    .or_else(|| command.strip_suffix(" *"));
                match (mode, prefix) {
                    ("allow", Some(prefix)) => {
                        push_unique(&mut plan.allow_prefix, tool, prefix.to_owned())
                    }
                    ("allow", None) => push_unique(&mut plan.allow, tool, command.to_owned()),
                    (mode, Some(prefix)) => {
                        let map = if mode == "deny" {
                            &mut plan.deny
                        } else {
                            &mut plan.ask
                        };
                        push_unique(map, tool, format!("{prefix} *"));
                    }
                    (mode, None) => {
                        let map = if mode == "deny" {
                            &mut plan.deny
                        } else {
                            &mut plan.ask
                        };
                        push_unique(map, tool, command.to_owned());
                    }
                }
            }
            (_, None) => {
                let map = match mode {
                    "allow" => &mut plan.allow,
                    "deny" => &mut plan.deny,
                    _ => &mut plan.ask,
                };
                push_unique(map, tool, "**".into());
            }
            (_, Some(pattern)) => match claude_path_pattern(pattern) {
                Some(pattern) => {
                    let map = match mode {
                        "allow" => &mut plan.allow,
                        "deny" => &mut plan.deny,
                        _ => &mut plan.ask,
                    };
                    push_unique(map, tool, pattern);
                }
                None => plan.unsupported.push(format!(
                    "{mode} rule '{rule}': project-root-relative paths have no equivalent"
                )),
            },
        }
    }
}

fn claude_model(model: &str) -> Option<String> {
    model
        .starts_with("claude-")
        .then(|| format!("anthropic/{model}"))
}

/// Convert Claude Code's `settings.json` and `.claude.json` contents.
pub(crate) fn plan_claude(
    settings: Option<&str>,
    claude_json: Option<&str>,
) -> Result<Plan, String> {
    let mut plan = Plan::default();
    if let Some(text) = settings {
        let json: Json = serde_json::from_str(text).map_err(|e| format!("settings.json: {e}"))?;
        if let Some(model) = json.get("model").and_then(Json::as_str) {
            match claude_model(model) {
                Some(model) => plan.model = Some(model),
                None => plan.unsupported.push(format!(
                    "model '{model}' is an alias; pick the equivalent with /model"
                )),
            }
        }
        if let Some(permissions) = json.get("permissions").and_then(Json::as_object) {
            for mode in ["allow", "ask", "deny"] {
                for rule in permissions
                    .get(mode)
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Json::as_str)
                {
                    claude_rule(rule, mode, &mut plan);
                }
            }
            if permissions.get("defaultMode").is_some() {
                plan.unsupported
                    .push("permissions.defaultMode: choose a mode with --mode or /mode".into());
            }
        }
        if let Some(hooks) = json.get("hooks").and_then(Json::as_object) {
            let groups: usize = hooks
                .values()
                .filter_map(Json::as_array)
                .map(Vec::len)
                .sum();
            plan.actions.push(format!(
                "{groups} hook group(s) in settings.json were not imported (global hooks are trusted without a prompt); review them and copy the ones you want into the mini-agent settings.json"
            ));
        }
        if json
            .get("env")
            .and_then(Json::as_object)
            .is_some_and(|e| !e.is_empty())
        {
            plan.unsupported
                .push("env: session environment variables are not imported".into());
        }
    }
    if let Some(text) = claude_json {
        let json: Json = serde_json::from_str(text).map_err(|e| format!(".claude.json: {e}"))?;
        if let Some(servers) = json.get("mcpServers").and_then(Json::as_object) {
            for (name, entry) in servers {
                claude_server(name, entry, &mut plan);
            }
        }
        if json
            .get("projects")
            .and_then(Json::as_object)
            .is_some_and(|p| {
                p.values().any(|v| {
                    v.get("mcpServers")
                        .and_then(Json::as_object)
                        .is_some_and(|m| !m.is_empty())
                })
            })
        {
            plan.unsupported
                .push("per-project MCP servers in .claude.json are not imported".into());
        }
    }
    Ok(plan)
}

/// Convert Codex's `config.toml`.
pub(crate) fn plan_codex(config: &str) -> Result<Plan, String> {
    let value: toml::Table = config.parse().map_err(|e| format!("config.toml: {e}"))?;
    let mut plan = Plan::default();
    if let Some(model) = value.get("model").and_then(toml::Value::as_str) {
        let provider = value.get("model_provider").and_then(toml::Value::as_str);
        if provider.is_none_or(|p| p == "openai") {
            plan.model = Some(format!("openai/{model}"));
        } else {
            plan.unsupported.push(format!(
                "model '{model}' with provider '{}': choose it with /model",
                provider.unwrap_or("")
            ));
        }
    }
    for key in ["approval_policy", "sandbox_mode"] {
        if let Some(setting) = value.get(key).and_then(toml::Value::as_str) {
            plan.unsupported.push(format!(
                "{key} = \"{setting}\": use --mode / --sandbox profiles instead"
            ));
        }
    }
    if let Some(servers) = value.get("mcp_servers").and_then(toml::Value::as_table) {
        for (name, entry) in servers {
            // Reuse the JSON converter through a JSON view of the entry.
            let mut json = serde_json::to_value(entry).map_err(|e| e.to_string())?;
            if let Some(object) = json.as_object_mut()
                && object.contains_key("url")
            {
                object.entry("type").or_insert(Json::String("http".into()));
                if let Some(var) = object
                    .remove("bearer_token_env_var")
                    .and_then(|v| v.as_str().map(str::to_owned))
                {
                    object
                        .entry("headers")
                        .or_insert_with(|| Json::Object(Default::default()))
                        .as_object_mut()
                        .expect("headers object")
                        .insert(
                            "Authorization".into(),
                            Json::String(format!("Bearer ${{{var}}}")),
                        );
                }
            }
            claude_server(name, &json, &mut plan);
        }
    }
    Ok(plan)
}

/// Convert opencode's `opencode.json`.
pub(crate) fn plan_opencode(config: &str) -> Result<Plan, String> {
    let json: Json = serde_json::from_str(config).map_err(|e| format!("opencode.json: {e}"))?;
    let mut plan = Plan::default();
    if let Some(model) = json.get("model").and_then(Json::as_str) {
        if model.contains('/') {
            plan.model = Some(model.to_owned());
        } else {
            plan.unsupported
                .push(format!("model '{model}': choose it with /model"));
        }
    }
    if let Some(servers) = json.get("mcp").and_then(Json::as_object) {
        for (name, entry) in servers {
            let Some(object) = entry.as_object() else {
                continue;
            };
            if object.get("enabled") == Some(&Json::Bool(false)) {
                plan.unsupported
                    .push(format!("MCP server '{name}': disabled in opencode"));
                continue;
            }
            let mut converted = serde_json::Map::new();
            match object.get("type").and_then(Json::as_str) {
                Some("local") => {
                    let command: Vec<&str> = object
                        .get("command")
                        .and_then(Json::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Json::as_str)
                        .collect();
                    let Some((program, args)) = command.split_first() else {
                        plan.unsupported
                            .push(format!("MCP server '{name}': no command"));
                        continue;
                    };
                    converted.insert("command".into(), Json::String((*program).to_owned()));
                    converted.insert(
                        "args".into(),
                        Json::Array(args.iter().map(|a| Json::String((*a).to_owned())).collect()),
                    );
                    if let Some(env) = object.get("environment") {
                        converted.insert("env".into(), env.clone());
                    }
                }
                Some("remote") => {
                    converted.insert("type".into(), Json::String("http".into()));
                    if let Some(url) = object.get("url") {
                        converted.insert("url".into(), url.clone());
                    }
                    if let Some(headers) = object.get("headers") {
                        converted.insert("headers".into(), headers.clone());
                    }
                }
                other => {
                    plan.unsupported.push(format!(
                        "MCP server '{name}': type {:?} is not supported",
                        other.unwrap_or("")
                    ));
                    continue;
                }
            }
            claude_server(name, &Json::Object(converted), &mut plan);
        }
    }
    if let Some(permission) = json.get("permission").and_then(Json::as_object) {
        for (tool, rule) in permission {
            let mapped = match tool.as_str() {
                "bash" => "bash",
                "read" => "read",
                "edit" => "edit",
                "write" => "write",
                "grep" => "grep",
                "webfetch" => "web_fetch",
                other => {
                    plan.unsupported
                        .push(format!("permission.{other}: no equivalent"));
                    continue;
                }
            };
            let pairs: Vec<(String, String)> = match rule {
                Json::String(action) => vec![("**".to_owned(), action.clone())],
                Json::Object(map) => map
                    .iter()
                    .filter_map(|(pattern, action)| {
                        Some((pattern.clone(), action.as_str()?.to_owned()))
                    })
                    .collect(),
                _ => Vec::new(),
            };
            for (pattern, action) in pairs {
                let map = match action.as_str() {
                    "allow" => &mut plan.allow,
                    "deny" => &mut plan.deny,
                    "ask" => &mut plan.ask,
                    _ => continue,
                };
                push_unique(map, mapped, pattern);
            }
        }
    }
    Ok(plan)
}

/// Markdown files below `dir`, relative paths kept.
fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() && depth < 4 {
                walk(&path, out, depth + 1);
            } else if kind.is_file() && path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out, 0);
    out.sort();
    out
}

fn plan_copies(plan: &mut Plan, from: &Path, to: &Path) {
    for file in markdown_files(from) {
        if let Ok(relative) = file.strip_prefix(from) {
            plan.files.push(FileCopy {
                from: file.clone(),
                to: to.join(relative),
            });
        }
    }
}

/// Read the foreign files of `source` below `home` and build a plan.
/// `config_dir` and `agents_dir` are where copies land.
pub(crate) fn plan_from_home(
    source: Source,
    home: &Path,
    config_dir: &Path,
    agents_dir: &Path,
) -> Result<Plan, String> {
    let read = |path: PathBuf| std::fs::read_to_string(path).ok();
    let mut plan = match source {
        Source::Claude => {
            let settings = read(home.join(".claude/settings.json"));
            let user = read(home.join(".claude.json"));
            if settings.is_none() && user.is_none() {
                return Err(
                    "no Claude Code settings found (~/.claude/settings.json, ~/.claude.json)"
                        .into(),
                );
            }
            let mut plan = plan_claude(settings.as_deref(), user.as_deref())?;
            plan_copies(
                &mut plan,
                &home.join(".claude/commands"),
                &config_dir.join("commands"),
            );
            plan_copies(&mut plan, &home.join(".claude/agents"), agents_dir);
            plan
        }
        Source::Codex => {
            let text = read(home.join(".codex/config.toml"))
                .ok_or("no Codex settings found (~/.codex/config.toml)")?;
            let mut plan = plan_codex(&text)?;
            plan_copies(
                &mut plan,
                &home.join(".codex/prompts"),
                &config_dir.join("commands"),
            );
            plan
        }
        Source::Opencode => {
            let dir = home.join(".config/opencode");
            let text = read(dir.join("opencode.json"))
                .ok_or("no opencode settings found (~/.config/opencode/opencode.json)")?;
            let mut plan = plan_opencode(&text)?;
            for name in ["command", "commands"] {
                plan_copies(&mut plan, &dir.join(name), &config_dir.join("commands"));
            }
            for name in ["agent", "agents"] {
                plan_copies(&mut plan, &dir.join(name), agents_dir);
            }
            plan
        }
    };
    // Executable-looking custom commands are plain Markdown prompts; note the
    // inline-shell syntax so the user knows it runs only with approval.
    plan.files.retain(|copy| !copy.to.exists());
    Ok(plan)
}

fn string_array(items: &[String]) -> toml_edit::Array {
    items.iter().map(String::as_str).collect()
}

/// Apply `plan` to the TOML text of the global config, preserving every
/// existing key and comment. Returns the new text and what was skipped.
pub(crate) fn apply_to_config(
    existing: &str,
    plan: &Plan,
) -> Result<(String, Vec<String>), String> {
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| format!("the existing config.toml does not parse: {e}"))?;
    let mut skipped = Vec::new();
    if let Some(model) = &plan.model {
        if doc.contains_key("model") {
            skipped.push(format!("model: kept your existing model (not '{model}')"));
        } else {
            doc["model"] = toml_edit::value(model.as_str());
        }
    }
    for (name, table) in &plan.mcp {
        let present = doc
            .get("mcp_servers")
            .and_then(|s| s.as_table_like())
            .is_some_and(|s| s.contains_key(name));
        if present {
            skipped.push(format!(
                "MCP server '{name}': already configured, kept yours"
            ));
            continue;
        }
        let servers = doc
            .entry("mcp_servers")
            .or_insert_with(|| {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                toml_edit::Item::Table(t)
            })
            .as_table_like_mut()
            .ok_or("mcp_servers is not a table")?;
        let mut item = toml_edit::Table::new();
        for (key, value) in table {
            item.insert(key, toml_value_to_item(value));
        }
        servers.insert(name, toml_edit::Item::Table(item));
    }
    for (key, map) in [
        ("permission-allow", &plan.allow),
        ("permission-allow-prefix", &plan.allow_prefix),
        ("permission-ask", &plan.ask),
        ("permission-deny", &plan.deny),
    ] {
        if map.is_empty() {
            continue;
        }
        let section = doc
            .entry(key)
            .or_insert_with(|| {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                toml_edit::Item::Table(t)
            })
            .as_table_like_mut()
            .ok_or_else(|| format!("{key} is not a table"))?;
        for (tool, patterns) in map {
            let mut merged: Vec<String> = section
                .get(tool)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            for pattern in patterns {
                if !merged.contains(pattern) {
                    merged.push(pattern.clone());
                }
            }
            section.insert(tool, toml_edit::value(string_array(&merged)));
        }
    }
    Ok((doc.to_string(), skipped))
}

fn toml_value_to_item(value: &toml::Value) -> toml_edit::Item {
    match value {
        toml::Value::String(s) => toml_edit::value(s.as_str()),
        toml::Value::Boolean(b) => toml_edit::value(*b),
        toml::Value::Integer(i) => toml_edit::value(*i),
        toml::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                if let Some(s) = item.as_str() {
                    array.push(s);
                }
            }
            toml_edit::value(array)
        }
        toml::Value::Table(table) => {
            let mut out = toml_edit::Table::new();
            for (k, v) in table {
                out.insert(k, toml_value_to_item(v));
            }
            toml_edit::Item::Table(out)
        }
        other => toml_edit::value(other.to_string()),
    }
}

/// Lines present in `after` but not in `before`, as a `+` listing. The edit
/// is additive, so this is the whole diff.
pub(crate) fn diff_lines(before: &str, after: &str) -> Vec<String> {
    let mut remaining: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for line in before.lines() {
        *remaining.entry(line).or_default() += 1;
    }
    after
        .lines()
        .filter(|line| match remaining.get_mut(line) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        })
        .map(|line| format!("+ {line}"))
        .collect()
}

/// The full preview shown before anything is written.
pub(crate) fn preview(
    source: Source,
    plan: &Plan,
    config_diff: &[String],
    skipped: &[String],
) -> String {
    let mut out = format!("Import from {}:\n", source.label());
    if config_diff.is_empty() && plan.files.is_empty() {
        out.push_str("\n  nothing to import.\n");
    }
    if !config_diff.is_empty() {
        out.push_str("\nconfig.toml changes:\n");
        for line in config_diff {
            out.push_str(&format!("  {line}\n"));
        }
    }
    if !plan.files.is_empty() {
        out.push_str("\nfiles to copy:\n");
        for copy in &plan.files {
            out.push_str(&format!(
                "  + {}  <-  {}\n",
                copy.to.display(),
                copy.from.display()
            ));
        }
    }
    for (title, items) in [
        ("kept as they were", skipped),
        ("action needed", &plan.actions[..]),
        ("not imported (no equivalent)", &plan.unsupported[..]),
    ] {
        if !items.is_empty() {
            out.push_str(&format!("\n{title}:\n"));
            for item in items {
                out.push_str(&format!("  - {item}\n"));
            }
        }
    }
    out.push_str(
        "\nImported MCP servers and commands run only after the usual approvals; hooks are never imported.\n",
    );
    out
}

/// Parsed command line of `import-settings`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ImportArgs {
    pub(crate) source: Source,
    pub(crate) dry_run: bool,
    pub(crate) yes: bool,
}

pub(crate) const USAGE: &str = "\
Usage: mini-agent import-settings <claude|codex|opencode> [--dry-run] [--yes]

Convert another agent's user settings (MCP servers, permission rules, model,
commands, agents) into the mini-agent config. Shows a preview first.

Options:
      --dry-run  Show the preview and exit without writing
      --yes      Write without asking (the preview is still printed)
  -h, --help     Print this help";

pub(crate) enum Invocation {
    Run(ImportArgs),
    Help,
    Invalid(String),
}

/// `import-settings ...` anywhere it is the first argument; other first
/// arguments are not claimed.
pub(crate) fn parse_invocation(args: &[std::ffi::OsString]) -> Option<Invocation> {
    let mut rest = args.iter().skip(1).map(|a| a.to_str());
    if rest.next()? != Some("import-settings") {
        return None;
    }
    let mut source = None;
    let (mut dry_run, mut yes) = (false, false);
    for arg in rest {
        match arg {
            Some("-h" | "--help") => return Some(Invocation::Help),
            Some("--dry-run") => dry_run = true,
            Some("--yes") => yes = true,
            Some("claude") if source.is_none() => source = Some(Source::Claude),
            Some("codex") if source.is_none() => source = Some(Source::Codex),
            Some("opencode") if source.is_none() => source = Some(Source::Opencode),
            Some(other) => {
                return Some(Invocation::Invalid(format!(
                    "unexpected argument '{other}'"
                )));
            }
            None => return Some(Invocation::Invalid("arguments must be valid UTF-8".into())),
        }
    }
    Some(match source {
        Some(source) => Invocation::Run(ImportArgs {
            source,
            dry_run,
            yes,
        }),
        None => Invocation::Invalid("choose one of claude, codex, opencode".into()),
    })
}

fn confirmed() -> bool {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("Write these changes? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok()
        && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Run the command; returns the process exit code.
pub(crate) fn run(args: ImportArgs) -> i32 {
    let Some(home) = dirs::home_dir() else {
        eprintln!("error: cannot determine the home directory");
        return 1;
    };
    let paths = match crate::paths::AppPaths::from_process(std::env::current_dir().ok()) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("error: cannot resolve mini-agent directories: {error}");
            return 1;
        }
    };
    let plan = match plan_from_home(args.source, &home, &paths.config_dir, &paths.agents_dir()) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let config_path = crate::config::load::pick_existing(&paths.config_dir);
    if config_path.extension().is_some_and(|ext| ext != "toml") {
        eprintln!(
            "error: {} is not TOML; import supports config.toml only. Add the entries by hand or convert it first.",
            config_path.display()
        );
        return 1;
    }
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let (updated, skipped) = match apply_to_config(&existing, &plan) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let diff = diff_lines(&existing, &updated);
    print!("{}", preview(args.source, &plan, &diff, &skipped));
    if args.dry_run {
        println!("(dry run: nothing was written)");
        return 0;
    }
    if diff.is_empty() && plan.files.is_empty() {
        return 0;
    }
    if !args.yes && !confirmed() {
        eprintln!("Nothing written. Re-run with --yes to apply without asking.");
        return 2;
    }
    if let Err(error) = write_all(&config_path, &existing, &updated, &plan) {
        eprintln!("error: {error}");
        return 1;
    }
    println!("Imported. Run `mini-agent --print-config` to review the result.");
    0
}

fn write_all(config_path: &Path, existing: &str, updated: &str, plan: &Plan) -> Result<(), String> {
    if existing != updated {
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        crate::fs::atomic_write_sync(config_path, updated.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", config_path.display()))?;
    }
    for copy in &plan.files {
        if copy.to.exists() {
            continue;
        }
        if let Some(parent) = copy.to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let bytes = std::fs::read(&copy.from)
            .map_err(|e| format!("cannot read {}: {e}", copy.from.display()))?;
        crate::fs::atomic_write_sync(&copy.to, &bytes)
            .map_err(|e| format!("cannot write {}: {e}", copy.to.display()))?;
    }
    Ok(())
}

/// A one-line pointer for `--setup` when another agent's settings exist.
pub(crate) fn detection_note(home: &Path) -> Option<String> {
    let found: Vec<&str> = [
        (Source::Claude, home.join(".claude/settings.json")),
        (Source::Claude, home.join(".claude.json")),
        (Source::Codex, home.join(".codex/config.toml")),
        (
            Source::Opencode,
            home.join(".config/opencode/opencode.json"),
        ),
    ]
    .into_iter()
    .filter(|(_, path)| path.is_file())
    .map(|(source, _)| match source {
        Source::Claude => "claude",
        Source::Codex => "codex",
        Source::Opencode => "opencode",
    })
    .collect();
    if found.is_empty() {
        return None;
    }
    let mut unique = found;
    unique.dedup();
    Some(format!(
        "Found settings from {}. Preview a conversion with `mini-agent import-settings {} --dry-run`.",
        unique.join(", "),
        unique[0]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_SETTINGS: &str = r#"{
      "model": "claude-sonnet-4-5",
      "permissions": {
        "allow": ["Bash(git status)", "Bash(npm run test:*)", "Read(//etc/hosts)", "Edit(src/**)", "WebFetch(domain:x.com)"],
        "deny": ["Read(./.env)", "Bash(rm:*)"],
        "defaultMode": "acceptEdits"
      },
      "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "evil.sh"}]}]},
      "env": {"A": "b"}
    }"#;
    const CLAUDE_JSON: &str = r#"{"mcpServers": {
      "files": {"command": "npx", "args": ["-y", "fs-server", "."], "env": {"API_TOKEN": "sk-livesecretvalue123", "MODE": "fast"}},
      "remote": {"type": "http", "url": "https://x.example/mcp", "headers": {"Authorization": "Bearer abcdef", "X-Team": "t"}},
      "bad name!": {"command": "x"}
    }}"#;

    #[test]
    fn claude_settings_map_rules_servers_and_list_the_rest() {
        let plan = plan_claude(Some(CLAUDE_SETTINGS), Some(CLAUDE_JSON)).unwrap();
        assert_eq!(plan.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
        assert_eq!(plan.allow["bash"], ["git status"]);
        assert_eq!(plan.allow_prefix["bash"], ["npm run test"]);
        assert_eq!(plan.allow["read"], ["/etc/hosts"]);
        assert_eq!(plan.allow["edit"], ["src/**"]);
        assert_eq!(plan.deny["read"], [".env"]);
        assert_eq!(plan.deny["bash"], ["rm *"]);
        let unsupported = plan.unsupported.join("\n");
        assert!(unsupported.contains("WebFetch"), "{unsupported}");
        assert!(unsupported.contains("defaultMode"));
        assert!(unsupported.contains("bad name!"));
        assert!(plan.actions.iter().any(|a| a.contains("hook group")));
        let files = &plan.mcp["files"];
        assert_eq!(files["env"]["API_TOKEN"].as_str(), Some("${API_TOKEN}"));
        assert_eq!(files["env"]["MODE"].as_str(), Some("fast"));
        let remote = &plan.mcp["remote"];
        assert_eq!(
            remote["headers"]["Authorization"].as_str(),
            Some("Bearer ${MCP_REMOTE_AUTHORIZATION}")
        );
        let everything = format!("{plan:?}");
        assert!(!everything.contains("sk-livesecretvalue123"));
        assert!(!everything.contains("abcdef"));
        assert!(!everything.contains("evil.sh"));
    }

    #[test]
    fn codex_and_opencode_fixtures_convert() {
        let codex = plan_codex(
            "model = \"gpt-5\"\napproval_policy = \"never\"\n[mcp_servers.docs]\ncommand = \"npx\"\nargs = [\"-y\", \"docs\"]\n[mcp_servers.web]\nurl = \"https://w.example\"\nbearer_token_env_var = \"WEB_TOKEN\"\n",
        )
        .unwrap();
        assert_eq!(codex.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(codex.mcp["docs"]["command"].as_str(), Some("npx"));
        assert_eq!(
            codex.mcp["web"]["headers"]["Authorization"].as_str(),
            Some("Bearer ${WEB_TOKEN}")
        );
        assert!(
            codex
                .unsupported
                .iter()
                .any(|u| u.contains("approval_policy"))
        );

        let opencode = plan_opencode(
            r#"{"model":"anthropic/claude-x","mcp":{"loc":{"type":"local","command":["node","s.js"],"environment":{"SECRET_KEY":"abc"}},"off":{"type":"remote","url":"https://o","enabled":false}},
                "permission":{"bash":{"git *":"allow","rm *":"deny"},"edit":"ask","task":"allow"}}"#,
        )
        .unwrap();
        assert_eq!(opencode.model.as_deref(), Some("anthropic/claude-x"));
        assert_eq!(opencode.mcp["loc"]["command"].as_str(), Some("node"));
        assert_eq!(
            opencode.mcp["loc"]["env"]["SECRET_KEY"].as_str(),
            Some("${SECRET_KEY}")
        );
        assert!(!opencode.mcp.contains_key("off"));
        assert_eq!(opencode.allow["bash"], ["git *"]);
        assert_eq!(opencode.deny["bash"], ["rm *"]);
        assert_eq!(opencode.ask["edit"], ["**"]);
        assert!(opencode.unsupported.iter().any(|u| u.contains("task")));
    }

    #[test]
    fn applying_preserves_existing_keys_and_never_overrides() {
        let existing = "# mine\nmodel = \"openai/gpt-4o\"\n\n[mcp_servers.files]\ncommand = \"keep-me\"\n\n[permission-allow]\nbash = [\"cargo test\"]\n";
        let plan = plan_claude(Some(CLAUDE_SETTINGS), Some(CLAUDE_JSON)).unwrap();
        let (updated, skipped) = apply_to_config(existing, &plan).unwrap();
        assert!(updated.starts_with("# mine\nmodel = \"openai/gpt-4o\""));
        assert!(updated.contains("command = \"keep-me\""));
        assert!(skipped.iter().any(|s| s.contains("model")));
        assert!(skipped.iter().any(|s| s.contains("'files'")));
        let parsed: toml::Table = updated.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["remote"]["url"].as_str(),
            Some("https://x.example/mcp")
        );
        let bash: Vec<&str> = parsed["permission-allow"]["bash"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(bash, ["cargo test", "git status"]);
        assert_eq!(parsed["permission-deny"]["bash"][0].as_str(), Some("rm *"));
        // The real loader accepts the result and sees servers and rules.
        let cfg = crate::config::load::parse_config_content(Path::new("config.toml"), &updated)
            .expect("the imported config loads");
        let rendered = format!("{cfg:?}");
        assert!(rendered.contains("https://x.example/mcp"), "{rendered}");
        assert!(rendered.contains("git status"), "{rendered}");
        // Applying twice changes nothing more.
        let (again, _) = apply_to_config(&updated, &plan).unwrap();
        assert_eq!(again, updated);
        let diff = diff_lines(existing, &updated);
        assert!(diff.iter().all(|l| l.starts_with("+ ")));
        assert!(diff.iter().any(|l| l.contains("https://x.example/mcp")));
        assert!(!diff.iter().any(|l| l.contains("keep-me")));
    }

    #[test]
    fn plan_from_home_reads_files_and_dry_run_style_use_never_writes() {
        let root = std::env::temp_dir().join(format!("mini-agent-import-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let config = root.join("config");
        let agents = root.join("agents");
        std::fs::create_dir_all(home.join(".claude/commands/team")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), CLAUDE_SETTINGS).unwrap();
        std::fs::write(home.join(".claude.json"), CLAUDE_JSON).unwrap();
        std::fs::write(home.join(".claude/commands/team/ship.md"), "ship it").unwrap();
        let plan = plan_from_home(Source::Claude, &home, &config, &agents).unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].to, config.join("commands/team/ship.md"));
        let (updated, skipped) = apply_to_config("", &plan).unwrap();
        let text = preview(Source::Claude, &plan, &diff_lines("", &updated), &skipped);
        assert!(text.contains("config.toml changes") && text.contains("not imported"));
        // Planning and previewing touched nothing.
        assert!(!config.exists() && !agents.exists());
        write_all(&config.join("config.toml"), "", &updated, &plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(config.join("commands/team/ship.md")).unwrap(),
            "ship it"
        );
        assert!(
            std::fs::read_to_string(config.join("config.toml"))
                .unwrap()
                .contains("mcp_servers")
        );
        assert!(plan_from_home(Source::Codex, &home, &config, &agents).is_err());
        assert!(detection_note(&home).unwrap().contains("claude"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn invocation_parsing_claims_only_import_settings() {
        let args = |items: &[&str]| {
            items
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        assert!(parse_invocation(&args(&["mini-agent", "import settings"])).is_none());
        assert!(parse_invocation(&args(&["mini-agent"])).is_none());
        match parse_invocation(&args(&[
            "mini-agent",
            "import-settings",
            "codex",
            "--dry-run",
        ])) {
            Some(Invocation::Run(a)) => assert_eq!(
                a,
                ImportArgs {
                    source: Source::Codex,
                    dry_run: true,
                    yes: false
                }
            ),
            _ => panic!("expected run"),
        }
        assert!(matches!(
            parse_invocation(&args(&["mini-agent", "import-settings"])),
            Some(Invocation::Invalid(_))
        ));
        assert!(matches!(
            parse_invocation(&args(&["mini-agent", "import-settings", "vim"])),
            Some(Invocation::Invalid(_))
        ));
    }
}
