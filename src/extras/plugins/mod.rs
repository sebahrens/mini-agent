//! Installable plugin bundles and marketplaces.
//!
//! `mini-agent plugin install|list|remove|update|trust` and
//! `plugin marketplace add|list|remove` manage plugins in Claude Code's layout
//! (see [`manifest`]). A plugin is copied (never linked) from a local path or
//! a git clone into `<data_dir>/plugins/<name>/` and recorded in
//! `installed.json` with a SHA-256 of its content.
//!
//! What becomes active, and when:
//!
//! * Commands are copied under `<config_dir>/commands/<plugin>/` and appear as
//!   `/<plugin>:<name>`; agents are written to `<data_dir>/agents/` as
//!   `<plugin>-<name>`; skills load from the installed directory (listed as
//!   `<plugin>:<name>`; a same-named skill of yours wins). These are prompts,
//!   not executables.
//! * Hooks and MCP servers are executable and stay **inactive** until the user
//!   runs `mini-agent plugin trust <name>`, which shows every command and asks
//!   for confirmation. Approval is bound to the content hash: an update that
//!   changes any file revokes it again. Nothing is ever trusted automatically.
//! * A marketplace is a JSON index (a local directory or file, or a git URL)
//!   registered in `marketplaces.json`; `install <plugin>@<marketplace>`
//!   resolves a plugin from it.
pub(crate) mod manifest;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use manifest::{Components, HookGroup};

/// Where a plugin came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Source {
    /// A local path or a git URL, as given or resolved from a marketplace.
    pub(crate) location: String,
    /// Requested branch or tag for git sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) git_ref: Option<String>,
    /// Commit a git source resolved to when it was installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) marketplace: Option<String>,
}

/// What `plugin trust` activated, kept so removal and revocation undo exactly it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Activated {
    pub(crate) mcp: Vec<String>,
    pub(crate) hooks: Vec<HookRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct HookRecord {
    pub(crate) event: String,
    pub(crate) group: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Installed {
    pub(crate) version: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) source: Source,
    pub(crate) hash: String,
    pub(crate) installed_at: String,
    /// Files this plugin wrote outside its own directory (commands, agents).
    pub(crate) files: Vec<PathBuf>,
    /// The content hash the user approved hooks and MCP servers for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) trusted_hash: Option<String>,
    #[serde(default)]
    pub(crate) activated: Activated,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    plugins: BTreeMap<String, Installed>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Marketplaces {
    #[serde(default)]
    marketplaces: BTreeMap<String, MarketplaceRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct MarketplaceRecord {
    source: String,
}

/// Directories the plugin manager works in.
pub(crate) struct Ctx {
    pub(crate) plugins_dir: PathBuf,
    pub(crate) config_dir: PathBuf,
    pub(crate) agents_dir: PathBuf,
}

/// How plugin sources are fetched: a git clone into a directory, returning the
/// commit it resolved to.
pub(crate) trait Fetcher {
    fn clone_git(
        &mut self,
        url: &str,
        git_ref: Option<&str>,
        dest: &Path,
    ) -> Result<String, String>;
}

fn new_staging(ctx: &Ctx) -> Result<PathBuf, String> {
    let dir = ctx.plugins_dir.join(format!(
        ".staging-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    std::fs::create_dir_all(&ctx.plugins_dir)
        .map_err(|e| format!("cannot create {}: {e}", ctx.plugins_dir.display()))?;
    std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

impl Ctx {
    fn registry_path(&self) -> PathBuf {
        self.plugins_dir.join("installed.json")
    }
    fn marketplaces_path(&self) -> PathBuf {
        self.plugins_dir.join("marketplaces.json")
    }
    fn plugin_dir(&self, name: &str) -> PathBuf {
        self.plugins_dir.join(name)
    }
    fn market_dir(&self, name: &str) -> PathBuf {
        self.plugins_dir.join("marketplaces").join(name)
    }
    fn commands_dir(&self) -> PathBuf {
        self.config_dir.join("commands")
    }
    fn settings_path(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }
    fn config_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    fn load_registry(&self) -> Registry {
        std::fs::read_to_string(self.registry_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save_registry(&self, registry: &Registry) -> Result<(), String> {
        std::fs::create_dir_all(&self.plugins_dir).map_err(|e| e.to_string())?;
        let json = serde_json::to_vec_pretty(registry).map_err(|e| e.to_string())?;
        crate::fs::atomic_write_sync(&self.registry_path(), &json)
            .map_err(|e| format!("cannot save the plugin list: {e}"))
    }

    fn load_markets(&self) -> Marketplaces {
        std::fs::read_to_string(self.marketplaces_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save_markets(&self, markets: &Marketplaces) -> Result<(), String> {
        std::fs::create_dir_all(&self.plugins_dir).map_err(|e| e.to_string())?;
        let json = serde_json::to_vec_pretty(markets).map_err(|e| e.to_string())?;
        crate::fs::atomic_write_sync(&self.marketplaces_path(), &json).map_err(|e| e.to_string())
    }
}

/// `https://`, `ssh://` and scp-style `user@host:path` git locations; nothing
/// that git would treat as a transport helper, a local file or an option.
pub(crate) fn is_git_url(spec: &str) -> bool {
    if spec.starts_with('-') || spec.contains(['\0', '\n', ' ']) || spec.contains("::") {
        return false;
    }
    spec.starts_with("https://")
        || spec.starts_with("ssh://")
        || (spec.contains('@')
            && spec.contains(':')
            && !spec.contains("://")
            && !spec.starts_with('/'))
}

/// A fetched plugin source: the directory to read and what to record.
struct Fetched {
    dir: PathBuf,
    source: Source,
    /// Staging directory to delete afterwards (git clones).
    scratch: Option<PathBuf>,
}

fn fetch(
    ctx: &Ctx,
    fetcher: &mut dyn Fetcher,
    spec: &str,
    git_ref: Option<&str>,
    cwd: &Path,
) -> Result<Fetched, String> {
    if is_git_url(spec) {
        let scratch = new_staging(ctx)?;
        let dest = scratch.join("clone");
        match fetcher.clone_git(spec, git_ref, &dest) {
            Ok(commit) => Ok(Fetched {
                dir: dest,
                source: Source {
                    location: spec.to_owned(),
                    git_ref: git_ref.map(str::to_owned),
                    commit: Some(commit),
                    marketplace: None,
                },
                scratch: Some(scratch),
            }),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&scratch);
                Err(error)
            }
        }
    } else {
        let path = if Path::new(spec).is_absolute() {
            PathBuf::from(spec)
        } else {
            cwd.join(spec)
        };
        let path = path.canonicalize().map_err(|e| format!("{spec}: {e}"))?;
        if !path.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        Ok(Fetched {
            dir: path.clone(),
            source: Source {
                location: path.to_string_lossy().into_owned(),
                git_ref: None,
                commit: None,
                marketplace: None,
            },
            scratch: None,
        })
    }
}

/// The result of an install or update, for display.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) name: String,
    pub(crate) commands: usize,
    pub(crate) agents: usize,
    pub(crate) skills: usize,
    pub(crate) hooks: usize,
    pub(crate) mcp: usize,
    pub(crate) changed: bool,
    pub(crate) retrust: bool,
}

impl Report {
    pub(crate) fn summary(&self) -> String {
        let mut text = format!(
            "{}: {} command(s), {} agent(s), {} skill(s), {} hook(s), {} MCP server(s)",
            self.name, self.commands, self.agents, self.skills, self.hooks, self.mcp
        );
        if self.hooks + self.mcp > 0 {
            text.push_str(&format!(
                "\n  hooks and MCP servers are inactive until you run: mini-agent plugin trust {}",
                self.name
            ));
        }
        if self.retrust {
            text.push_str("\n  the content changed, so your earlier approval was revoked");
        }
        text
    }
}

fn rewrite_agent_name(content: &str, new_name: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(str::to_owned).collect();
    if lines.first().map(String::as_str) == Some("---")
        && let Some(end) = lines.iter().skip(1).position(|l| l == "---")
    {
        for line in lines.iter_mut().skip(1).take(end) {
            if line.starts_with("name:") {
                *line = format!("name: {new_name}");
            }
        }
    }
    let mut out = lines.join("\n");
    if content.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Install `dir` (a plugin root) under `source`.
fn install_dir(ctx: &Ctx, dir: &Path, source: Source) -> Result<Report, String> {
    let (manifest, json) = manifest::read_manifest(dir)?;
    let name = manifest.name.clone();
    let mut registry = ctx.load_registry();
    if registry.plugins.contains_key(&name) {
        return Err(format!(
            "plugin '{name}' is already installed (use `plugin update {name}`)"
        ));
    }
    let staging = new_staging(ctx)?;
    let staged = staging.join("plugin");
    let result = (|| -> Result<(Report, Installed), String> {
        manifest::copy_tree(dir, &staged)?;
        let hash = manifest::content_hash(&staged)?;
        let found = manifest::discover(&staged, &json)?;
        let final_dir = ctx.plugin_dir(&name);
        if final_dir.exists() {
            return Err(format!("{} already exists", final_dir.display()));
        }
        // Plan every file this plugin writes outside its own directory first,
        // so a conflict aborts before anything is written.
        let mut writes: Vec<(PathBuf, String)> = Vec::new();
        for command in &found.commands {
            let rel: PathBuf = command.name.split(':').collect();
            let dest = ctx
                .commands_dir()
                .join(&name)
                .join(rel)
                .with_extension("md");
            writes.push((
                dest,
                std::fs::read_to_string(staged.join(&command.rel)).map_err(|e| e.to_string())?,
            ));
        }
        for agent in &found.agents {
            let agent_name = format!("{name}-{}", agent.name);
            if !crate::context::agents::valid_agent_name(&agent_name) {
                continue;
            }
            let content =
                std::fs::read_to_string(staged.join(&agent.rel)).map_err(|e| e.to_string())?;
            writes.push((
                ctx.agents_dir.join(format!("{agent_name}.md")),
                rewrite_agent_name(&content, &agent_name),
            ));
        }
        if let Some((existing, _)) = writes.iter().find(|(path, _)| path.exists()) {
            return Err(format!(
                "{} already exists; refusing to overwrite it",
                existing.display()
            ));
        }
        std::fs::rename(&staged, &final_dir)
            .map_err(|e| format!("cannot install into {}: {e}", final_dir.display()))?;
        let mut files = Vec::new();
        for (path, content) in &writes {
            let outcome = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .map_err(|e| e.to_string())
                .and_then(|_| {
                    crate::fs::atomic_write_sync(path, content.as_bytes())
                        .map_err(|e| e.to_string())
                });
            if let Err(error) = outcome {
                for done in &files {
                    let _ = std::fs::remove_file(done);
                }
                let _ = std::fs::remove_dir_all(&final_dir);
                return Err(format!("cannot write {}: {error}", path.display()));
            }
            files.push(path.clone());
        }
        let report = Report {
            name: name.clone(),
            commands: found.commands.len(),
            agents: found.agents.len(),
            skills: found.skills.len(),
            hooks: found.hooks.len(),
            mcp: found.mcp.len(),
            changed: true,
            retrust: false,
        };
        let installed = Installed {
            version: manifest.version.clone(),
            description: manifest.description.clone(),
            source,
            hash,
            installed_at: crate::session::rfc3339_utc(jiff::Timestamp::now()),
            files,
            trusted_hash: None,
            activated: Activated::default(),
        };
        Ok((report, installed))
    })();
    let _ = std::fs::remove_dir_all(&staging);
    let (report, installed) = result?;
    registry.plugins.insert(name, installed);
    ctx.save_registry(&registry)?;
    Ok(report)
}

/// Components of an installed plugin, read from its directory.
fn components_of(ctx: &Ctx, name: &str) -> Result<Components, String> {
    let dir = ctx.plugin_dir(name);
    let (_, json) = manifest::read_manifest(&dir)?;
    manifest::discover(&dir, &json)
}

fn substitute_root(value: &Value, root: &Path) -> Value {
    match value {
        Value::String(text) => Value::String(
            text.replace("${CLAUDE_PLUGIN_ROOT}", &root.to_string_lossy())
                .replace("${MINI_AGENT_PLUGIN_ROOT}", &root.to_string_lossy()),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| substitute_root(v, root)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_root(v, root)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The text shown before approval: every hook command and MCP server.
pub(crate) fn trust_preview(name: &str, components: &Components, root: &Path) -> String {
    let mut out = format!("Plugin '{name}' wants to activate executable components:\n");
    for HookGroup { event, group } in &components.hooks {
        let group = substitute_root(group, root);
        let matcher = group.get("matcher").and_then(Value::as_str).unwrap_or("*");
        for handler in group
            .get("hooks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let command = handler
                .get("command")
                .and_then(Value::as_str)
                .or_else(|| handler.get("url").and_then(Value::as_str))
                .unwrap_or("(non-command handler)");
            let args: Vec<&str> = handler
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            out.push_str(&format!(
                "  hook  {event} [{matcher}]: {command} {}\n",
                args.join(" ")
            ));
        }
    }
    for (server, def) in &components.mcp {
        let def = substitute_root(def, root);
        let target = def
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let args: Vec<&str> = def
                    .get("args")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                format!(
                    "{} {}",
                    def.get("command").and_then(Value::as_str).unwrap_or("?"),
                    args.join(" ")
                )
            });
        let env: Vec<&str> = def
            .get("env")
            .and_then(Value::as_object)
            .map(|e| e.keys().map(String::as_str).collect())
            .unwrap_or_default();
        out.push_str(&format!("  mcp   {name}.{server}: {target}"));
        if !env.is_empty() {
            out.push_str(&format!("  (env names: {})", env.join(", ")));
        }
        out.push('\n');
    }
    out
}

fn mcp_server_key(plugin: &str, server: &str) -> String {
    format!("{plugin}.{server}")
}

/// Turn authored MCP JSON into the TOML table the global config uses.
/// Credential-looking values are replaced by `${NAME}` references.
fn mcp_table(key: &str, def: &Value, root: &Path) -> toml::Table {
    let def = substitute_root(def, root);
    let mut plan = crate::setup::import::Plan::default();
    crate::setup::import::claude_server(key, &def, &mut plan);
    plan.mcp.remove(key).unwrap_or_default()
}

fn read_settings(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} is not valid JSON ({e}); not touching it",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Default::default())),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn add_hooks(settings: &mut Value, groups: &[HookRecord]) -> Result<(), String> {
    let root = settings
        .as_object_mut()
        .ok_or("settings.json is not an object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Default::default()))
        .as_object_mut()
        .ok_or("settings.json hooks is not an object")?;
    for record in groups {
        let list = hooks
            .entry(record.event.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or("a hooks event is not an array")?;
        if !list.contains(&record.group) {
            list.push(record.group.clone());
        }
    }
    Ok(())
}

fn remove_hooks(settings: &mut Value, groups: &[HookRecord]) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for record in groups {
        if let Some(list) = hooks.get_mut(&record.event).and_then(Value::as_array_mut) {
            list.retain(|group| group != &record.group);
        }
    }
    hooks.retain(|_, list| list.as_array().is_none_or(|l| !l.is_empty()));
    if hooks.is_empty() {
        settings.as_object_mut().map(|o| o.remove("hooks"));
    }
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    crate::fs::atomic_write_sync(path, &bytes)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Undo what `trust` activated for `name`.
fn deactivate(ctx: &Ctx, activated: &Activated) -> Result<(), String> {
    if !activated.hooks.is_empty() {
        let path = ctx.settings_path();
        let mut settings = read_settings(&path)?;
        remove_hooks(&mut settings, &activated.hooks);
        write_json(&path, &settings)?;
    }
    if !activated.mcp.is_empty() {
        let path = ctx.config_path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            let mut doc: toml_edit::DocumentMut = text
                .parse()
                .map_err(|e| format!("{} does not parse: {e}", path.display()))?;
            if let Some(servers) = doc
                .get_mut("mcp_servers")
                .and_then(|s| s.as_table_like_mut())
            {
                for key in &activated.mcp {
                    servers.remove(key);
                }
            }
            crate::fs::atomic_write_sync(&path, doc.to_string().as_bytes())
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Approve and activate the plugin's hooks and MCP servers.
pub(crate) fn trust(
    ctx: &Ctx,
    name: &str,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> Result<String, String> {
    let mut registry = ctx.load_registry();
    let record = registry
        .plugins
        .get_mut(name)
        .ok_or_else(|| format!("plugin '{name}' is not installed"))?;
    let root = ctx.plugin_dir(name);
    let current = manifest::content_hash(&root)?;
    if current != record.hash {
        return Err(format!(
            "{name}: files changed since install; run `plugin update {name}` or reinstall"
        ));
    }
    let components = components_of(ctx, name)?;
    if components.hooks.is_empty() && components.mcp.is_empty() {
        return Ok(format!("{name} has no hooks or MCP servers to approve."));
    }
    if record.trusted_hash.as_deref() == Some(current.as_str()) {
        return Ok(format!("{name} is already approved for this content."));
    }
    let preview = trust_preview(name, &components, &root);
    if !confirm(&preview) {
        return Err("not approved; nothing was activated".into());
    }
    // Hooks: global settings.json.
    let hook_records: Vec<HookRecord> = components
        .hooks
        .iter()
        .map(|h| HookRecord {
            event: h.event.clone(),
            group: substitute_root(&h.group, &root),
        })
        .collect();
    if !hook_records.is_empty() {
        let path = ctx.settings_path();
        let mut settings = read_settings(&path)?;
        add_hooks(&mut settings, &hook_records)?;
        write_json(&path, &settings)?;
    }
    // MCP servers: global config.toml (`<plugin>.<server>`).
    let mut mcp_keys = Vec::new();
    if !components.mcp.is_empty() {
        let mut plan = crate::setup::import::Plan::default();
        for (server, def) in &components.mcp {
            let key = mcp_server_key(name, server);
            plan.mcp.insert(key.clone(), mcp_table(&key, def, &root));
            mcp_keys.push(key);
        }
        let path = ctx.config_path();
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let (updated, _skipped) = crate::setup::import::apply_to_config(&existing, &plan)?;
        std::fs::create_dir_all(&ctx.config_dir).map_err(|e| e.to_string())?;
        crate::fs::atomic_write_sync(&path, updated.as_bytes()).map_err(|e| e.to_string())?;
    }
    record.trusted_hash = Some(current);
    record.activated = Activated {
        mcp: mcp_keys,
        hooks: hook_records,
    };
    let summary = format!(
        "Approved {name}: {} hook(s), {} MCP server(s) active (restart mini-agent to load them).",
        record.activated.hooks.len(),
        record.activated.mcp.len()
    );
    ctx.save_registry(&registry)?;
    Ok(summary)
}

/// Remove a plugin: its files, copied commands and agents, and any activation.
pub(crate) fn remove(ctx: &Ctx, name: &str) -> Result<(), String> {
    let mut registry = ctx.load_registry();
    let record = registry
        .plugins
        .get(name)
        .cloned()
        .ok_or_else(|| format!("plugin '{name}' is not installed"))?;
    deactivate(ctx, &record.activated)?;
    for file in &record.files {
        let _ = std::fs::remove_file(file);
        // Drop now-empty namespace directories below commands/.
        let mut dir = file.parent();
        while let Some(d) = dir {
            if d == ctx.commands_dir()
                || !d.starts_with(ctx.commands_dir())
                || std::fs::remove_dir(d).is_err()
            {
                break;
            }
            dir = d.parent();
        }
    }
    let _ = std::fs::remove_dir_all(ctx.plugin_dir(name));
    registry.plugins.remove(name);
    ctx.save_registry(&registry)
}

/// Install from a path or git URL.
pub(crate) fn install(
    ctx: &Ctx,
    fetcher: &mut dyn Fetcher,
    spec: &str,
    git_ref: Option<&str>,
    cwd: &Path,
) -> Result<Report, String> {
    if let Some((plugin, market)) = spec.split_once('@')
        && !is_git_url(spec)
        && manifest::valid_plugin_name(plugin)
        && !market.is_empty()
        && !spec.contains('/')
    {
        return install_from_marketplace(ctx, fetcher, plugin, market, cwd);
    }
    let fetched = fetch(ctx, fetcher, spec, git_ref, cwd)?;
    let result = install_dir(ctx, &fetched.dir, fetched.source);
    if let Some(scratch) = fetched.scratch {
        let _ = std::fs::remove_dir_all(scratch);
    }
    result
}

/// Re-fetch an installed plugin from its recorded source; replace it when its
/// content changed (revoking any approval).
pub(crate) fn update(
    ctx: &Ctx,
    fetcher: &mut dyn Fetcher,
    name: &str,
    cwd: &Path,
) -> Result<Report, String> {
    let registry = ctx.load_registry();
    let record = registry
        .plugins
        .get(name)
        .cloned()
        .ok_or_else(|| format!("plugin '{name}' is not installed"))?;
    let fetched = fetch(
        ctx,
        fetcher,
        &record.source.location,
        record.source.git_ref.as_deref(),
        cwd,
    )?;
    let result = (|| {
        let staging = new_staging(ctx)?;
        let probe = staging.join("probe");
        let hash =
            manifest::copy_tree(&fetched.dir, &probe).and_then(|_| manifest::content_hash(&probe));
        let _ = std::fs::remove_dir_all(&staging);
        let hash = hash?;
        if hash == record.hash {
            let components = components_of(ctx, name)?;
            return Ok(Report {
                name: name.to_owned(),
                commands: components.commands.len(),
                agents: components.agents.len(),
                skills: components.skills.len(),
                hooks: components.hooks.len(),
                mcp: components.mcp.len(),
                changed: false,
                retrust: false,
            });
        }
        let was_trusted = record.trusted_hash.is_some();
        remove(ctx, name)?;
        let mut source = fetched.source.clone();
        source.marketplace = record.source.marketplace.clone();
        let mut report = install_dir(ctx, &fetched.dir, source)?;
        report.retrust = was_trusted;
        Ok(report)
    })();
    if let Some(scratch) = fetched.scratch {
        let _ = std::fs::remove_dir_all(scratch);
    }
    result
}

// ---- marketplaces --------------------------------------------------------

fn index_path(dir: &Path) -> Option<PathBuf> {
    [
        dir.join(".claude-plugin/marketplace.json"),
        dir.join("marketplace.json"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// Add a marketplace from a local directory/file or a git URL.
pub(crate) fn marketplace_add(
    ctx: &Ctx,
    fetcher: &mut dyn Fetcher,
    spec: &str,
    cwd: &Path,
) -> Result<String, String> {
    let fetched = fetch(ctx, fetcher, spec, None, cwd);
    let (dir, source, scratch) = match fetched {
        Ok(f) => (f.dir, f.source.location, f.scratch),
        Err(error) => {
            // A single JSON file is also a marketplace.
            let path = if Path::new(spec).is_absolute() {
                PathBuf::from(spec)
            } else {
                cwd.join(spec)
            };
            if path.is_file() {
                let staging = new_staging(ctx)?;
                std::fs::copy(&path, staging.join("marketplace.json"))
                    .map_err(|e| e.to_string())?;
                (
                    staging.clone(),
                    path.to_string_lossy().into_owned(),
                    Some(staging),
                )
            } else {
                return Err(error);
            }
        }
    };
    let result = (|| {
        let index = index_path(&dir)
            .ok_or("no .claude-plugin/marketplace.json (or marketplace.json) found")?;
        let json = std::fs::read_to_string(&index).map_err(|e| e.to_string())?;
        let value: Value =
            serde_json::from_str(&json).map_err(|e| format!("marketplace.json: {e}"))?;
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .ok_or("marketplace.json needs a name")?;
        if !manifest::valid_plugin_name(name) {
            return Err(format!("marketplace name '{name}' is not usable"));
        }
        let dest = ctx.market_dir(name);
        let _ = std::fs::remove_dir_all(&dest);
        manifest::copy_tree(&dir, &dest)?;
        let mut markets = ctx.load_markets();
        markets
            .marketplaces
            .insert(name.to_owned(), MarketplaceRecord { source });
        ctx.save_markets(&markets)?;
        let count = value
            .get("plugins")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        Ok(format!("Added marketplace '{name}' ({count} plugin(s))."))
    })();
    if let Some(scratch) = scratch {
        let _ = std::fs::remove_dir_all(scratch);
    }
    result
}

pub(crate) fn marketplace_remove(ctx: &Ctx, name: &str) -> Result<(), String> {
    let mut markets = ctx.load_markets();
    if markets.marketplaces.remove(name).is_none() {
        return Err(format!("no marketplace named '{name}'"));
    }
    ctx.save_markets(&markets)?;
    let _ = std::fs::remove_dir_all(ctx.market_dir(name));
    Ok(())
}

/// `(plugin, description)` rows of a registered marketplace.
pub(crate) fn marketplace_plugins(
    ctx: &Ctx,
    market: &str,
) -> Result<Vec<(String, String)>, String> {
    let dir = ctx.market_dir(market);
    let index = index_path(&dir).ok_or_else(|| format!("no marketplace named '{market}'"))?;
    let value: Value =
        serde_json::from_str(&std::fs::read_to_string(index).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    Ok(value
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some((
                p.get("name")?.as_str()?.to_owned(),
                p.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect(),
            ))
        })
        .collect())
}

fn install_from_marketplace(
    ctx: &Ctx,
    fetcher: &mut dyn Fetcher,
    plugin: &str,
    market: &str,
    cwd: &Path,
) -> Result<Report, String> {
    if !ctx.load_markets().marketplaces.contains_key(market) {
        return Err(format!(
            "no marketplace named '{market}' (add it with `plugin marketplace add`)"
        ));
    }
    let dir = ctx.market_dir(market);
    let index = index_path(&dir).ok_or("the marketplace index is missing")?;
    let value: Value =
        serde_json::from_str(&std::fs::read_to_string(index).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let entry = value
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(plugin))
        .ok_or_else(|| format!("marketplace '{market}' has no plugin '{plugin}'"))?;
    let (spec, git_ref) = match entry.get("source") {
        Some(Value::String(rel)) => {
            let rel = PathBuf::from(rel.trim_start_matches("./"));
            if rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err("the marketplace source path must stay inside the marketplace".into());
            }
            (dir.join(rel).to_string_lossy().into_owned(), None)
        }
        Some(Value::Object(object)) => {
            let git_ref = object.get("ref").and_then(Value::as_str).map(str::to_owned);
            match object.get("source").and_then(Value::as_str) {
                Some("github") => {
                    let repo = object.get("repo").and_then(Value::as_str).unwrap_or("");
                    if repo.split('/').count() != 2
                        || repo.contains(['\0', ' ', '.']) && repo.contains("..")
                    {
                        return Err("the marketplace names an invalid GitHub repository".into());
                    }
                    (format!("https://github.com/{repo}.git"), git_ref)
                }
                Some("url" | "git") => (
                    object
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    git_ref,
                ),
                _ => return Err("the marketplace entry has an unsupported source".into()),
            }
        }
        _ => return Err("the marketplace entry has no source".into()),
    };
    let fetched = fetch(ctx, fetcher, &spec, git_ref.as_deref(), cwd)?;
    let mut source = fetched.source;
    source.marketplace = Some(market.to_owned());
    let result = install_dir(ctx, &fetched.dir, source);
    if let Some(scratch) = fetched.scratch {
        let _ = std::fs::remove_dir_all(scratch);
    }
    result
}

// ---- listing -------------------------------------------------------------

/// One line per installed plugin, then its components, for `plugin list` and
/// `/plugins`.
pub(crate) fn describe(ctx: &Ctx) -> String {
    let registry = ctx.load_registry();
    if registry.plugins.is_empty() {
        return "No plugins installed. Install one with `mini-agent plugin install <path|git-url|name@marketplace>`.".into();
    }
    let mut out = String::new();
    for (name, record) in &registry.plugins {
        let state = match &record.trusted_hash {
            Some(hash) if *hash == record.hash => "approved",
            _ => "not approved",
        };
        out.push_str(&format!(
            "{name} {} ({})\n",
            record.version.as_deref().unwrap_or("(no version)"),
            record.source.location
        ));
        if let Ok(components) = components_of(ctx, name) {
            let list = |label: &str, names: Vec<String>| {
                if names.is_empty() {
                    String::new()
                } else {
                    format!("  {label}: {}\n", names.join(", "))
                }
            };
            out.push_str(&list(
                "commands",
                components
                    .commands
                    .iter()
                    .map(|c| format!("/{name}:{}", c.name))
                    .collect(),
            ));
            out.push_str(&list(
                "agents",
                components
                    .agents
                    .iter()
                    .map(|a| format!("{name}-{}", a.name))
                    .collect(),
            ));
            out.push_str(&list(
                "skills",
                components
                    .skills
                    .iter()
                    .map(|s| format!("{name}:{}", s.name))
                    .collect(),
            ));
            if !components.hooks.is_empty() || !components.mcp.is_empty() {
                out.push_str(&format!(
                    "  hooks: {}, MCP servers: {} ({state})\n",
                    components.hooks.len(),
                    components.mcp.len()
                ));
            }
        }
    }
    out
}

/// Installed plugin directories that carry skills, for `skills_plugin_dirs`.
pub(crate) fn installed_skill_dirs(plugins_dir: &Path) -> Vec<PathBuf> {
    let registry: Registry = std::fs::read_to_string(plugins_dir.join("installed.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    registry
        .plugins
        .keys()
        .map(|name| plugins_dir.join(name))
        .filter(|dir| dir.join("skills").is_dir())
        .collect()
}

pub(crate) fn describe_for_paths(paths: &crate::paths::AppPaths) -> String {
    describe(&Ctx {
        plugins_dir: paths.data_dir.join("plugins"),
        config_dir: paths.config_dir.clone(),
        agents_dir: paths.agents_dir(),
    })
}

// ---- command line --------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Install {
        spec: String,
        git_ref: Option<String>,
    },
    List,
    Remove(String),
    Update(Option<String>),
    Trust {
        name: String,
        yes: bool,
    },
    MarketplaceAdd(String),
    MarketplaceList,
    MarketplaceRemove(String),
}

pub(crate) const USAGE: &str = "\
Usage: mini-agent plugin <command>

Commands:
  install <path|git-url|name@marketplace> [--ref REF]   Install a plugin
  list                                   Show installed plugins and components
  remove <name>                          Remove a plugin and what it wrote
  update [name]                          Re-fetch from the recorded source
  trust <name> [--yes]                   Review and approve its hooks and MCP servers
  marketplace add <path|git-url>         Register a marketplace index
  marketplace list|remove <name>         List its plugins / unregister it

Hooks and MCP servers stay inactive until `plugin trust`; the approval is bound
to the plugin's content hash and revoked when an update changes it.";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Invocation {
    Run(Command),
    Help,
    Invalid(String),
}

pub(crate) fn parse_invocation(args: &[std::ffi::OsString]) -> Option<Invocation> {
    let words: Vec<Option<&str>> = args.iter().map(|a| a.to_str()).collect();
    if words.get(1) != Some(&Some("plugin")) {
        return None;
    }
    let sub = words.get(2).copied().flatten()?;
    if !matches!(
        sub,
        "install" | "list" | "remove" | "update" | "trust" | "marketplace" | "-h" | "--help"
    ) {
        return None;
    }
    let Some(rest) = words[3..].iter().copied().collect::<Option<Vec<&str>>>() else {
        return Some(Invocation::Invalid("arguments must be valid UTF-8".into()));
    };
    let invalid = |text: &str| Some(Invocation::Invalid(text.to_owned()));
    match (sub, rest.as_slice()) {
        ("-h" | "--help", _) => Some(Invocation::Help),
        ("list", []) => Some(Invocation::Run(Command::List)),
        ("remove", [name]) => Some(Invocation::Run(Command::Remove((*name).to_owned()))),
        ("update", []) => Some(Invocation::Run(Command::Update(None))),
        ("update", [name]) => Some(Invocation::Run(Command::Update(Some((*name).to_owned())))),
        ("trust", [name]) => Some(Invocation::Run(Command::Trust {
            name: (*name).to_owned(),
            yes: false,
        })),
        ("trust", [name, "--yes"]) | ("trust", ["--yes", name]) => {
            Some(Invocation::Run(Command::Trust {
                name: (*name).to_owned(),
                yes: true,
            }))
        }
        ("install", [spec]) if !spec.starts_with('-') => Some(Invocation::Run(Command::Install {
            spec: (*spec).to_owned(),
            git_ref: None,
        })),
        ("install", [spec, "--ref", git_ref])
            if !spec.starts_with('-') && !git_ref.starts_with('-') =>
        {
            Some(Invocation::Run(Command::Install {
                spec: (*spec).to_owned(),
                git_ref: Some((*git_ref).to_owned()),
            }))
        }
        ("marketplace", ["add", spec]) if !spec.starts_with('-') => {
            Some(Invocation::Run(Command::MarketplaceAdd((*spec).to_owned())))
        }
        ("marketplace", ["list"]) => Some(Invocation::Run(Command::MarketplaceList)),
        ("marketplace", ["remove", name]) => Some(Invocation::Run(Command::MarketplaceRemove(
            (*name).to_owned(),
        ))),
        _ => invalid(&format!("invalid arguments for plugin {sub}")),
    }
}

/// Clones with the system git: https/ssh only, no prompts, no submodules,
/// bounded by a timeout, output logged to a scratch file.
struct GitFetcher;

impl Fetcher for GitFetcher {
    fn clone_git(
        &mut self,
        url: &str,
        git_ref: Option<&str>,
        dest: &Path,
    ) -> Result<String, String> {
        use crate::extras::schedule::launch_process_with_env;
        let log = dest.with_extension("log");
        let env = [("GIT_TERMINAL_PROMPT", "0")];
        let mut argv: Vec<String> = [
            "git",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "protocol.file.allow=never",
            "clone",
            "--depth",
            "1",
            "--no-tags",
            "--single-branch",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        if let Some(git_ref) = git_ref {
            argv.extend(["--branch".into(), git_ref.to_owned()]);
        }
        argv.extend([
            "--".into(),
            url.to_owned(),
            dest.to_string_lossy().into_owned(),
        ]);
        match launch_process_with_env(&argv, None, Some(&log), Duration::from_secs(180), &env)? {
            (Some(0), false) => {}
            (_, true) => return Err("git clone timed out".into()),
            (code, _) => {
                return Err(format!(
                    "git clone failed ({})",
                    code.map_or("signal".into(), |c| format!("exit {c}"))
                ));
            }
        }
        let argv: Vec<String> = ["git", "-C", &dest.to_string_lossy(), "rev-parse", "HEAD"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let rev_log = dest.with_extension("rev");
        launch_process_with_env(&argv, None, Some(&rev_log), Duration::from_secs(20), &env)?;
        let rev = std::fs::read_to_string(&rev_log)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if rev.len() < 7 || !rev.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("could not resolve the cloned commit".into());
        }
        Ok(rev)
    }
}

fn confirm_on_terminal(preview: &str) -> bool {
    use std::io::{BufRead, IsTerminal, Write};
    println!("{preview}");
    if !std::io::stdin().is_terminal() {
        eprintln!("Not a terminal; re-run with --yes to approve.");
        return false;
    }
    print!("Approve these? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok()
        && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub(crate) fn run(command: Command) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = match crate::paths::AppPaths::from_process(Some(cwd.clone())) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("error: cannot resolve mini-agent directories: {error}");
            return 1;
        }
    };
    let ctx = Ctx {
        plugins_dir: paths.data_dir.join("plugins"),
        config_dir: paths.config_dir.clone(),
        agents_dir: paths.agents_dir(),
    };
    let fail = |error: String| {
        eprintln!("error: {error}");
        1
    };
    match command {
        Command::Install { spec, git_ref } => {
            match install(&ctx, &mut GitFetcher, &spec, git_ref.as_deref(), &cwd) {
                Ok(report) => {
                    println!("Installed {}", report.summary());
                    0
                }
                Err(error) => fail(error),
            }
        }
        Command::List => {
            print!("{}", describe(&ctx));
            0
        }
        Command::Remove(name) => match remove(&ctx, &name) {
            Ok(()) => {
                println!("Removed {name}.");
                0
            }
            Err(error) => fail(error),
        },
        Command::Update(name) => {
            let names: Vec<String> = match name {
                Some(name) => vec![name],
                None => ctx.load_registry().plugins.keys().cloned().collect(),
            };
            let mut code = 0;
            for name in names {
                match update(&ctx, &mut GitFetcher, &name, &cwd) {
                    Ok(report) if report.changed => println!("Updated {}", report.summary()),
                    Ok(_) => println!("{name} is up to date."),
                    Err(error) => {
                        eprintln!("error: {name}: {error}");
                        code = 1;
                    }
                }
            }
            code
        }
        Command::Trust { name, yes } => {
            let mut confirm = |preview: &str| {
                if yes {
                    println!("{preview}");
                    true
                } else {
                    confirm_on_terminal(preview)
                }
            };
            match trust(&ctx, &name, &mut confirm) {
                Ok(message) => {
                    println!("{message}");
                    0
                }
                Err(error) => fail(error),
            }
        }
        Command::MarketplaceAdd(spec) => {
            match marketplace_add(&ctx, &mut GitFetcher, &spec, &cwd) {
                Ok(message) => {
                    println!("{message}");
                    0
                }
                Err(error) => fail(error),
            }
        }
        Command::MarketplaceList => {
            let markets = ctx.load_markets();
            if markets.marketplaces.is_empty() {
                println!(
                    "No marketplaces. Add one with `mini-agent plugin marketplace add <path|git-url>`."
                );
            }
            for (name, record) in markets.marketplaces {
                println!("{name}  ({})", record.source);
                for (plugin, description) in marketplace_plugins(&ctx, &name).unwrap_or_default() {
                    println!("  {plugin}@{name}  {description}");
                }
            }
            0
        }
        Command::MarketplaceRemove(name) => match marketplace_remove(&ctx, &name) {
            Ok(()) => {
                println!("Removed marketplace {name}.");
                0
            }
            Err(error) => fail(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest::fixture::write_plugin;

    struct NoGit;
    impl Fetcher for NoGit {
        fn clone_git(&mut self, _: &str, _: Option<&str>, _: &Path) -> Result<String, String> {
            Err("no network in tests".into())
        }
    }

    /// Writes a plugin tree into the destination, as a clone would.
    struct FakeGit(&'static str);
    impl Fetcher for FakeGit {
        fn clone_git(&mut self, _: &str, _: Option<&str>, dest: &Path) -> Result<String, String> {
            write_plugin(dest, self.0);
            Ok("0123456789abcdef0123456789abcdef01234567".into())
        }
    }

    fn fixture() -> (PathBuf, Ctx) {
        let root =
            std::env::temp_dir().join(format!("mini-agent-plugins-{}", uuid::Uuid::new_v4()));
        let ctx = Ctx {
            plugins_dir: root.join("data/plugins"),
            config_dir: root.join("config"),
            agents_dir: root.join("data/agents"),
        };
        std::fs::create_dir_all(&root).unwrap();
        (root, ctx)
    }

    #[test]
    fn install_loads_all_five_namespaced_and_nothing_executable_is_active() {
        let (root, ctx) = fixture();
        write_plugin(&root.join("src/acme"), "acme");
        let report = install(&ctx, &mut NoGit, "src/acme", None, &root).unwrap();
        assert_eq!(
            (
                report.commands,
                report.agents,
                report.skills,
                report.hooks,
                report.mcp
            ),
            (1, 1, 1, 1, 1)
        );
        // Commands and agents are namespaced files.
        assert!(root.join("config/commands/acme/ship.md").is_file());
        let agent = std::fs::read_to_string(root.join("data/agents/acme-reviewer.md")).unwrap();
        assert!(agent.contains("name: acme-reviewer"), "{agent}");
        // The skill is discoverable from the installed directory.
        assert_eq!(
            installed_skill_dirs(&ctx.plugins_dir),
            vec![ctx.plugin_dir("acme")]
        );
        let listing = describe(&ctx);
        assert!(
            listing.contains("/acme:ship")
                && listing.contains("acme-reviewer")
                && listing.contains("acme:lint")
        );
        // Hooks and MCP stay inactive until trust.
        assert!(!ctx.settings_path().exists() && !ctx.config_path().exists());
        assert!(listing.contains("not approved"));
        assert!(report.summary().contains("plugin trust acme"));
        // A second install of the same name is refused.
        assert!(install(&ctx, &mut NoGit, "src/acme", None, &root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trust_requires_confirmation_activates_exactly_the_reviewed_components_and_remove_undoes_all()
    {
        let (root, ctx) = fixture();
        write_plugin(&root.join("src/acme"), "acme");
        std::fs::create_dir_all(&ctx.config_dir).unwrap();
        std::fs::write(ctx.config_path(), "model = \"openai/x\"\n").unwrap();
        std::fs::write(ctx.settings_path(), "{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"mine\",\"args\":[]}]}]}}").unwrap();
        install(&ctx, &mut NoGit, "src/acme", None, &root).unwrap();

        // Declining activates nothing.
        let mut shown = String::new();
        assert!(
            trust(&ctx, "acme", &mut |preview| {
                shown = preview.to_owned();
                false
            })
            .is_err()
        );
        assert!(
            shown.contains("PreToolUse")
                && shown.contains("guard.sh")
                && shown.contains("acme.tools")
        );
        assert!(!shown.contains("sk-secret-value"), "{shown}");
        assert!(
            !std::fs::read_to_string(ctx.config_path())
                .unwrap()
                .contains("acme.tools")
        );

        trust(&ctx, "acme", &mut |_| true).unwrap();
        let config = std::fs::read_to_string(ctx.config_path()).unwrap();
        assert!(config.starts_with("model = \"openai/x\""));
        assert!(config.contains("[mcp_servers.\"acme.tools\"]"), "{config}");
        let reparsed: toml::Table = config.parse().unwrap();
        assert!(
            reparsed["mcp_servers"]
                .as_table()
                .unwrap()
                .contains_key("acme.tools")
        );
        assert!(!config.contains("sk-secret-value"), "{config}");
        let root_dir = ctx.plugin_dir("acme").to_string_lossy().into_owned();
        assert!(config.contains(&root_dir), "{config}");
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(ctx.settings_path()).unwrap()).unwrap();
        assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert!(
            settings["hooks"]["PreToolUse"][0]
                .to_string()
                .contains(&root_dir)
        );
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(describe(&ctx).contains("approved") && !describe(&ctx).contains("not approved"));

        remove(&ctx, "acme").unwrap();
        assert!(!ctx.plugin_dir("acme").exists());
        assert!(!root.join("config/commands/acme").exists());
        assert!(!root.join("data/agents/acme-reviewer.md").exists());
        assert!(
            !std::fs::read_to_string(ctx.config_path())
                .unwrap()
                .contains("acme")
        );
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(ctx.settings_path()).unwrap()).unwrap();
        assert!(settings["hooks"].get("PreToolUse").is_none());
        assert_eq!(
            settings["hooks"]["Stop"].as_array().unwrap().len(),
            1,
            "the user's own hook stays"
        );
        assert!(describe(&ctx).starts_with("No plugins installed"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_update_that_changes_content_revokes_the_approval() {
        let (root, ctx) = fixture();
        write_plugin(&root.join("src/acme"), "acme");
        install(&ctx, &mut NoGit, "src/acme", None, &root).unwrap();
        trust(&ctx, "acme", &mut |_| true).unwrap();
        let same = update(&ctx, &mut NoGit, "acme", &root).unwrap();
        assert!(!same.changed);
        assert!(ctx.load_registry().plugins["acme"].trusted_hash.is_some());
        std::fs::write(root.join("src/acme/guard.sh"), "#!/bin/sh\nexit 1\n").unwrap();
        let changed = update(&ctx, &mut NoGit, "acme", &root).unwrap();
        assert!(changed.changed && changed.retrust);
        assert!(ctx.load_registry().plugins["acme"].trusted_hash.is_none());
        let settings = std::fs::read_to_string(ctx.settings_path()).unwrap();
        assert!(!settings.contains("guard.sh"), "{settings}");
        // Tampering with installed files blocks a stale approval.
        std::fs::write(ctx.plugin_dir("acme").join("guard.sh"), "evil").unwrap();
        assert!(
            trust(&ctx, "acme", &mut |_| true)
                .unwrap_err()
                .contains("changed")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_sources_and_marketplaces_resolve_and_record_the_commit() {
        let (root, ctx) = fixture();
        std::fs::create_dir_all(root.join("market/.claude-plugin")).unwrap();
        write_plugin(&root.join("market/plugins/local-one"), "local-one");
        std::fs::write(
            root.join("market/.claude-plugin/marketplace.json"),
            r#"{"name":"team","plugins":[
                {"name":"local-one","source":"./plugins/local-one","description":"from the index"},
                {"name":"remote-one","source":{"source":"github","repo":"o/r"}},
                {"name":"evil","source":"../outside"}]}"#,
        )
        .unwrap();
        let message = marketplace_add(&ctx, &mut NoGit, "market", &root).unwrap();
        assert!(message.contains("team") && message.contains("3 plugin"));
        assert_eq!(marketplace_plugins(&ctx, "team").unwrap()[0].0, "local-one");
        let local = install(&ctx, &mut NoGit, "local-one@team", None, &root).unwrap();
        assert_eq!(local.name, "local-one");
        assert_eq!(
            ctx.load_registry().plugins["local-one"]
                .source
                .marketplace
                .as_deref(),
            Some("team")
        );
        assert!(install(&ctx, &mut NoGit, "evil@team", None, &root).is_err());
        assert!(install(&ctx, &mut NoGit, "missing@team", None, &root).is_err());
        assert!(install(&ctx, &mut NoGit, "x@nomarket", None, &root).is_err());
        // The github entry goes through the git fetcher.
        let remote = install(
            &ctx,
            &mut FakeGit("remote-one"),
            "remote-one@team",
            None,
            &root,
        )
        .unwrap();
        assert_eq!(remote.name, "remote-one");
        let record = &ctx.load_registry().plugins["remote-one"];
        assert_eq!(record.source.location, "https://github.com/o/r.git");
        assert_eq!(
            record.source.commit.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        // Direct git install with a ref.
        let direct = install(
            &ctx,
            &mut FakeGit("direct-one"),
            "https://example.com/x.git",
            Some("v1"),
            &root,
        )
        .unwrap();
        assert_eq!(
            ctx.load_registry().plugins[&direct.name]
                .source
                .git_ref
                .as_deref(),
            Some("v1")
        );
        marketplace_remove(&ctx, "team").unwrap();
        assert!(marketplace_plugins(&ctx, "team").is_err());
        // No scratch directories are left behind.
        assert!(
            std::fs::read_dir(&ctx.plugins_dir)
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().starts_with(".staging"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_locations_are_restricted_and_commands_parse() {
        for ok in [
            "https://github.com/o/r.git",
            "ssh://git@host/o/r",
            "git@github.com:o/r.git",
        ] {
            assert!(is_git_url(ok), "{ok}");
        }
        for bad in [
            "--upload-pack=x",
            "ext::sh -c x",
            "file:///etc",
            "/local/path",
            "http://insecure/x",
            "a b",
        ] {
            assert!(!is_git_url(bad), "{bad}");
        }
        let args = |line: &str| {
            std::iter::once("mini-agent")
                .chain(line.split_whitespace())
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        assert!(parse_invocation(&args("plugin the release")).is_none());
        assert_eq!(
            parse_invocation(&args("plugin install ./p --ref v2")),
            Some(Invocation::Run(Command::Install {
                spec: "./p".into(),
                git_ref: Some("v2".into())
            }))
        );
        assert_eq!(
            parse_invocation(&args("plugin trust acme --yes")),
            Some(Invocation::Run(Command::Trust {
                name: "acme".into(),
                yes: true
            }))
        );
        assert!(matches!(
            parse_invocation(&args("plugin install --evil")),
            Some(Invocation::Invalid(_))
        ));
        assert_eq!(
            parse_invocation(&args("plugin marketplace list")),
            Some(Invocation::Run(Command::MarketplaceList))
        );
    }
}
