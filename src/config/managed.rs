//! Administrator-managed policy layer.
//!
//! A root-owned `managed-config.toml` next to the managed hook settings is read
//! once at startup and enforced after every other source: user and project
//! configuration, CLI flags and prompt directives cannot widen what it
//! restricts. It carries a documented subset, not the full `config.toml`:
//!
//! ```toml
//! [permissions]
//! deny = { shell = ["rm -rf *"], write = ["/etc/**"] }  # appended to permission-deny
//! allowed_modes = ["standard", "guarded", "readonly"]   # selectable security modes
//! disable_yolo = true                                   # shorthand: drop `yolo` from the modes
//! require_permission_checks = true                      # refuse --dangerously-skip-permissions
//!                                                       # (implied by deny, allowed_modes, disable_yolo)
//!
//! [sandbox]
//! required = true                                       # force the sandbox on
//! max_profile = "workspace"                             # ceiling: workspace | host-read | host-full
//! max_network = "allowlist"                             # ceiling: deny | allowlist | allow
//! allowed_hosts = ["registry.npmjs.org"]                # pin sandbox-network-hosts; no run-time approvals
//! forbid_home_mounts = true                             # ignore sandbox-home-mounts
//! allowed_roots = ["/opt/toolchains"]                   # with max_profile: extra read/write/toolchain roots permitted
//! # Any sandbox pin implies `required = true`.
//!
//! [mcp]
//! disabled = false
//! allowed_servers = ["github"]                          # absent = no allowlist; a bare name matches the name only
//! # entries may be tables that also pin what runs:
//! # { name = "gh", command = "npx", args = ["-y", "gh-mcp"] }  or  { name = "docs", url = "https://docs.example/mcp" }
//! denied_servers = ["scratch"]
//!
//! [providers]
//! allowed = ["anthropic", "openai"]                     # absent = no allowlist
//! denied = ["openrouter"]
//! ```
//!
//! A present but unreadable, invalid, symlinked or non-root-writable file is a
//! startup error: the layer fails closed instead of being skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::permission::SecurityMode;

const MAX_MANAGED_BYTES: u64 = 256 * 1024;
const MAX_ENTRIES: usize = 512;
const MAX_PATTERN_BYTES: usize = 1024;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct PermissionsPolicy {
    deny: BTreeMap<String, Vec<String>>,
    allowed_modes: Option<Vec<String>>,
    disable_yolo: bool,
    require_permission_checks: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct SandboxPolicy {
    required: bool,
    /// Widest filesystem profile a user may select.
    max_profile: Option<crate::sandbox::SandboxProfile>,
    /// Widest network reach a user may select (`deny` < `allowlist` < `allow`).
    max_network: Option<crate::sandbox::SandboxNetwork>,
    /// Host pin: user `sandbox-network-hosts` entries must fall inside it and
    /// unlisted hosts are never offered for approval. Absent = no pin.
    allowed_hosts: Option<Vec<String>>,
    /// Drop every `sandbox-home-mounts` entry.
    forbid_home_mounts: bool,
    /// With `max_profile`: absolute directories the explicit
    /// `sandbox-read-roots`, `sandbox-write-roots`, `sandbox-toolchain-roots`
    /// and Windows AppContainer roots may fall inside. Without this key, or
    /// for roots outside it, those user grants are cleared.
    allowed_roots: Option<Vec<String>>,
}

/// One `allowed_servers` / `denied_servers` entry: a bare name (matches the
/// server name only) or a table whose present fields must all match.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum McpEntry {
    Name(String),
    Spec(Box<McpEntrySpec>),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct McpEntrySpec {
    name: Option<String>,
    /// Stdio servers: the exact `command`.
    command: Option<String>,
    /// With `command`: the exact argument list. Omitted means no arguments for
    /// an allow entry and any arguments for a deny entry.
    args: Option<Vec<String>>,
    /// URL servers: the exact `url` (a trailing `/` is ignored).
    url: Option<String>,
    /// With `command`: the exact `env` table. Omitted means no `env` for an
    /// allow entry (a user `env` such as `NODE_OPTIONS` would change what runs).
    env: Option<BTreeMap<String, String>>,
    /// With `command`: the exact working directory. Omitted means none.
    cwd: Option<String>,
    /// With `command`: the exact `inherit_env` names. Omitted means none.
    inherit_env: Option<Vec<String>>,
    /// With `command`: the required `sandbox` backend. Omitted allows any
    /// value, including none (an unsandboxed launch): pin it to forbid that.
    sandbox: Option<String>,
    /// With `url`: the exact `headers` table. Omitted allows any headers (they
    /// only reach the pinned URL); a `headers_helper` is refused either way.
    headers: Option<BTreeMap<String, String>>,
}

/// Permission rule keys for tools that modify file contents. A managed deny on
/// any of them applies to all of them (`js/write_file` follows `write`).
const WRITE_FAMILY: [&str; 3] = ["write", "edit", "notebook_edit"];

impl McpEntry {
    fn texts(&self) -> Vec<&str> {
        match self {
            Self::Name(name) => vec![name],
            Self::Spec(spec) => spec
                .name
                .iter()
                .chain(&spec.command)
                .chain(&spec.url)
                .chain(spec.args.iter().flatten())
                .chain(&spec.cwd)
                .chain(&spec.sandbox)
                .chain(spec.inherit_env.iter().flatten())
                .chain(spec.env.iter().flatten().flat_map(|(k, v)| [k, v]))
                .chain(spec.headers.iter().flatten().flat_map(|(k, v)| [k, v]))
                .map(String::as_str)
                .collect(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        let Self::Spec(spec) = self else {
            return Ok(());
        };
        if spec.name.is_none() && spec.command.is_none() && spec.url.is_none() {
            return Err("an MCP entry table needs name, command or url".into());
        }
        if spec.command.is_some() && spec.url.is_some() {
            return Err("an MCP entry cannot pin both command and url".into());
        }
        if spec.args.is_some() && spec.command.is_none() {
            return Err("an MCP entry sets args without command".into());
        }
        if spec.command.is_none()
            && (spec.env.is_some()
                || spec.cwd.is_some()
                || spec.inherit_env.is_some()
                || spec.sandbox.is_some())
        {
            return Err(
                "an MCP entry sets env, cwd, inherit_env or sandbox without command".into(),
            );
        }
        if spec.url.is_none() && spec.headers.is_some() {
            return Err("an MCP entry sets headers without url".into());
        }
        Ok(())
    }

    /// The launch-shaping fields of a pinned stdio entry. `strict` (allow
    /// entries) makes an omitted field mean "empty / unset"; a deny entry only
    /// compares the fields it lists.
    #[cfg(feature = "mcp")]
    fn launch_matches(
        spec: &McpEntrySpec,
        strict: bool,
        cwd: Option<&Path>,
        env: &std::collections::HashMap<String, String>,
        inherit_env: &[String],
        sandbox: Option<&str>,
    ) -> bool {
        if spec.command.is_none() {
            return true;
        }
        let env_ok = match &spec.env {
            Some(pinned) => {
                pinned.len() == env.len() && pinned.iter().all(|(k, v)| env.get(k) == Some(v))
            }
            None => !strict || env.is_empty(),
        };
        let cwd_ok = match &spec.cwd {
            Some(pinned) => cwd == Some(Path::new(pinned)),
            None => !strict || cwd.is_none(),
        };
        let inherit_ok = match &spec.inherit_env {
            Some(pinned) => pinned.as_slice() == inherit_env,
            None => !strict || inherit_env.is_empty(),
        };
        env_ok
            && cwd_ok
            && inherit_ok
            && spec
                .sandbox
                .as_deref()
                .is_none_or(|pinned| sandbox == Some(pinned))
    }

    /// Whether this entry matches the server `name` defined as `config`.
    /// `strict_args` makes an omitted `args` mean "no arguments".
    #[cfg(feature = "mcp")]
    fn matches(
        &self,
        name: &str,
        config: &crate::extras::mcp::config::McpServerConfig,
        strict_args: bool,
    ) -> bool {
        use crate::extras::mcp::config::McpServerConfig;
        let spec = match self {
            Self::Name(entry) => return entry == name,
            Self::Spec(spec) => spec,
        };
        if spec.name.as_deref().is_some_and(|entry| entry != name) {
            return false;
        }
        match config {
            // Built-in registrations carry a fixed, trusted identity.
            McpServerConfig::BuiltIn { .. } => spec.command.is_none() && spec.url.is_none(),
            McpServerConfig::Command {
                command,
                args,
                cwd,
                env,
                inherit_env,
                sandbox,
                ..
            } => {
                spec.url.is_none()
                    && spec.headers.is_none()
                    && Self::launch_matches(
                        spec,
                        strict_args,
                        cwd.as_deref(),
                        env,
                        inherit_env,
                        sandbox.as_deref(),
                    )
                    && spec.command.as_deref().is_none_or(|entry| entry == command)
                    && match (&spec.command, &spec.args) {
                        (Some(_), Some(entry)) => entry == args,
                        (Some(_), None) => !strict_args || args.is_empty(),
                        _ => true,
                    }
            }
            McpServerConfig::Url {
                url,
                headers,
                options,
                ..
            } => {
                spec.command.is_none()
                    && !(strict_args && spec.url.is_some() && options.headers_helper.is_some())
                    && spec.headers.as_ref().is_none_or(|pinned| {
                        pinned.len() == headers.len()
                            && pinned.iter().all(|(k, v)| headers.get(k) == Some(v))
                    })
                    && spec.url.as_deref().is_none_or(|entry| {
                        entry.trim_end_matches('/') == url.trim_end_matches('/')
                    })
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct McpPolicy {
    disabled: bool,
    allowed_servers: Option<Vec<McpEntry>>,
    denied_servers: Vec<McpEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ProvidersPolicy {
    allowed: Option<Vec<String>>,
    denied: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ManagedFile {
    permissions: PermissionsPolicy,
    sandbox: SandboxPolicy,
    mcp: McpPolicy,
    providers: ProvidersPolicy,
}

/// The validated managed policy and the file it came from.
#[derive(Debug, Clone)]
pub(crate) struct ManagedPolicy {
    path: PathBuf,
    file: ManagedFile,
    allowed_modes: Vec<SecurityMode>,
}

#[cfg(target_os = "macos")]
pub(crate) fn managed_config_path() -> PathBuf {
    PathBuf::from("/Library/Application Support/mini-agent/managed-config.toml")
}

#[cfg(target_os = "windows")]
pub(crate) fn managed_config_path() -> PathBuf {
    PathBuf::from(r"C:\ProgramData\mini-agent\managed-config.toml")
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn managed_config_path() -> PathBuf {
    PathBuf::from("/etc/mini-agent/managed-config.toml")
}

static ACTIVE: OnceLock<Option<ManagedPolicy>> = OnceLock::new();

/// Load the managed file once for this process. Call before configuration is
/// read; a second call returns the first result.
pub(crate) fn install() -> Result<Option<&'static ManagedPolicy>, String> {
    if let Some(active) = ACTIVE.get() {
        return Ok(active.as_ref());
    }
    let loaded = ManagedPolicy::load(&managed_config_path(), true)?;
    Ok(ACTIVE.get_or_init(|| loaded).as_ref())
}

/// The policy installed by [`install`], if a managed file exists.
pub(crate) fn active() -> Option<&'static ManagedPolicy> {
    ACTIVE.get().and_then(Option::as_ref)
}

/// Refusal text when `mode` is not selectable under the active policy.
pub(crate) fn mode_violation(mode: SecurityMode) -> Option<String> {
    active().and_then(|policy| policy.mode_violation(mode))
}

/// `mode`, lowered to the nearest permitted one under the active policy.
pub(crate) fn clamp_mode(mode: SecurityMode) -> SecurityMode {
    active().map_or(mode, |policy| policy.clamp_mode(mode))
}

/// Refuse an MCP server the active policy does not allow.
#[cfg(feature = "mcp")]
pub(crate) fn check_mcp_server(
    name: &str,
    config: &crate::extras::mcp::config::McpServerConfig,
) -> Result<(), String> {
    match active() {
        Some(policy) => policy.check_mcp_server(name, config),
        None => Ok(()),
    }
}

/// Refuse a provider the active policy does not allow.
pub(crate) fn check_provider(name: &str) -> Result<(), String> {
    match active() {
        Some(policy) => policy.check_provider(name),
        None => Ok(()),
    }
}

/// Canonical form of the longest existing ancestor plus the remaining
/// components, so a symlink inside an allowed root cannot name a target
/// outside it while not-yet-created paths still compare.
fn normalized_root(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut base = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(base) {
            return tail.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                base = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn bounded_list(what: &str, list: &[String]) -> Result<(), String> {
    if list.len() > MAX_ENTRIES || list.iter().any(|item| item.len() > MAX_PATTERN_BYTES) {
        return Err(format!("{what} exceeds the size limit"));
    }
    Ok(())
}

impl ManagedPolicy {
    /// Read and validate `path`. `Ok(None)` when it does not exist. With
    /// `require_root`, a non-root owner or a group/world-writable file or
    /// directory is refused (Unix); tests pass `false`.
    pub(crate) fn load(path: &Path, require_root: bool) -> Result<Option<Self>, String> {
        let shown = path.display();
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "managed policy {shown} cannot be inspected: {error}"
                ));
            }
        };
        if !metadata.file_type().is_file() {
            return Err(format!("managed policy {shown} is not a regular file"));
        }
        if metadata.len() > MAX_MANAGED_BYTES {
            return Err(format!("managed policy {shown} is larger than 256 KiB"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if require_root && metadata.uid() != 0 {
                return Err(format!(
                    "managed policy {shown} must be owned by root (uid {} found)",
                    metadata.uid()
                ));
            }
            if metadata.mode() & 0o022 != 0 {
                return Err(format!(
                    "managed policy {shown} must not be group- or world-writable"
                ));
            }
            if require_root && let Some(parent) = path.parent() {
                let parent_meta = std::fs::metadata(parent)
                    .map_err(|error| format!("managed policy directory is unreadable: {error}"))?;
                if parent_meta.uid() != 0 || parent_meta.mode() & 0o022 != 0 {
                    return Err(format!(
                        "managed policy directory of {shown} must be root-owned and not group- or world-writable"
                    ));
                }
            }
        }
        #[cfg(not(unix))]
        let _ = require_root;
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("managed policy {shown} cannot be read: {error}"))?;
        super::layers::validate_toml_bounds(&text)
            .map_err(|error| format!("managed policy {shown} is invalid: {error}"))?;
        let file: ManagedFile = toml::from_str(&text)
            .map_err(|error| format!("managed policy {shown} is invalid: {}", error.message()))?;
        Self::from_file(path.to_path_buf(), file).map(Some)
    }

    fn from_file(path: PathBuf, file: ManagedFile) -> Result<Self, String> {
        let shown = path.display();
        let permissions = &file.permissions;
        if permissions.deny.len() > MAX_ENTRIES {
            return Err(format!("managed policy {shown}: too many deny entries"));
        }
        for patterns in permissions.deny.values() {
            bounded_list("a deny entry", patterns)
                .map_err(|e| format!("managed policy {shown}: {e}"))?;
        }
        for (what, list) in [
            ("mcp.denied_servers", Some(&file.mcp.denied_servers)),
            ("mcp.allowed_servers", file.mcp.allowed_servers.as_ref()),
        ] {
            let Some(list) = list else { continue };
            if list.len() > MAX_ENTRIES
                || list
                    .iter()
                    .flat_map(McpEntry::texts)
                    .any(|text| text.len() > MAX_PATTERN_BYTES)
            {
                return Err(format!(
                    "managed policy {shown}: {what} exceeds the size limit"
                ));
            }
            for entry in list {
                entry
                    .validate()
                    .map_err(|e| format!("managed policy {shown}: {what}: {e}"))?;
            }
        }
        for (what, list) in [
            ("providers.allowed", file.providers.allowed.as_ref()),
            ("providers.denied", Some(&file.providers.denied)),
            ("sandbox.allowed_hosts", file.sandbox.allowed_hosts.as_ref()),
            ("sandbox.allowed_roots", file.sandbox.allowed_roots.as_ref()),
        ] {
            if let Some(list) = list {
                bounded_list(what, list).map_err(|e| format!("managed policy {shown}: {e}"))?;
            }
        }
        if let Some(hosts) = &file.sandbox.allowed_hosts {
            crate::sandbox::net_proxy::parse_rules(hosts)
                .map_err(|e| format!("managed policy {shown}: sandbox.allowed_hosts: {e}"))?;
        }
        if let Some(roots) = &file.sandbox.allowed_roots {
            if file.sandbox.max_profile.is_none() {
                return Err(format!(
                    "managed policy {shown}: sandbox.allowed_roots needs sandbox.max_profile"
                ));
            }
            if let Some(bad) = roots.iter().find(|root| !Path::new(root).is_absolute()) {
                return Err(format!(
                    "managed policy {shown}: sandbox.allowed_roots entry `{bad}` must be an absolute path"
                ));
            }
        }
        let mut allowed: Vec<SecurityMode> = match &permissions.allowed_modes {
            None => SecurityMode::all().collect(),
            Some(names) => {
                let mut modes = Vec::new();
                for name in names {
                    let mode = SecurityMode::from_str(name).ok_or_else(|| {
                        format!(
                            "managed policy {shown}: unknown mode in permissions.allowed_modes (accepted: {})",
                            SecurityMode::NAMES.join(", ")
                        )
                    })?;
                    if !modes.contains(&mode) {
                        modes.push(mode);
                    }
                }
                modes
            }
        };
        if permissions.disable_yolo {
            allowed.retain(|mode| *mode != SecurityMode::Yolo);
        }
        if allowed.is_empty() {
            return Err(format!(
                "managed policy {shown}: permissions.allowed_modes leaves no selectable mode"
            ));
        }
        Ok(Self {
            path,
            file,
            allowed_modes: allowed,
        })
    }

    /// The file this policy was read from.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `"<what> is refused by the managed policy <file>"`.
    pub(crate) fn refusal(&self, what: &str) -> String {
        format!(
            "{what} is refused by the managed policy {} (an administrator controls this setting)",
            self.path.display()
        )
    }

    pub(crate) fn mode_allowed(&self, mode: SecurityMode) -> bool {
        self.allowed_modes.contains(&mode)
    }

    pub(crate) fn mode_violation(&self, mode: SecurityMode) -> Option<String> {
        (!self.mode_allowed(mode)).then(|| {
            self.refusal(&format!(
                "security mode `{mode}` (permitted: {})",
                self.allowed_modes
                    .iter()
                    .map(|mode| mode.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }

    /// `mode` when permitted, else the most privileged permitted mode that
    /// grants no more than it, else the least privileged permitted mode.
    pub(crate) fn clamp_mode(&self, mode: SecurityMode) -> SecurityMode {
        if self.mode_allowed(mode) {
            return mode;
        }
        let rank = |mode: &SecurityMode| mode.privilege_rank();
        self.allowed_modes
            .iter()
            .filter(|candidate| rank(candidate) <= rank(&mode))
            .max_by_key(|candidate| rank(candidate))
            .or_else(|| {
                self.allowed_modes
                    .iter()
                    .min_by_key(|candidate| rank(candidate))
            })
            .copied()
            .unwrap_or(SecurityMode::ReadOnly)
    }

    pub(crate) fn permitted_modes(&self) -> &[SecurityMode] {
        &self.allowed_modes
    }

    /// True when the sandbox cannot be turned off: asked for explicitly, or
    /// implied by any pin (a pin on a disabled sandbox would constrain nothing).
    pub(crate) fn sandbox_required(&self) -> bool {
        self.file.sandbox.required || self.sandbox_pinned()
    }

    fn sandbox_pinned(&self) -> bool {
        let sandbox = &self.file.sandbox;
        sandbox.max_profile.is_some()
            || sandbox.max_network.is_some()
            || sandbox.allowed_hosts.is_some()
            || sandbox.forbid_home_mounts
            || sandbox.allowed_roots.is_some()
    }

    /// Whether an explicit read/write/toolchain root is permitted. Without a
    /// `max_profile` ceiling roots are unmanaged; with one, only roots inside
    /// `allowed_roots` pass.
    pub(crate) fn sandbox_root_allowed(&self, root: &Path) -> bool {
        let sandbox = &self.file.sandbox;
        if sandbox.max_profile.is_none() {
            return true;
        }
        let Some(allowed) = &sandbox.allowed_roots else {
            return false;
        };
        if !root.is_absolute()
            || root
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return false;
        }
        let root = normalized_root(root);
        allowed
            .iter()
            .any(|entry| root.starts_with(normalized_root(Path::new(entry))))
    }

    /// Refusal when a CLI-supplied sandbox root falls outside the ceiling.
    pub(crate) fn sandbox_root_violation(&self, flag: &str, root: &Path) -> Option<String> {
        (!self.sandbox_root_allowed(root)).then(|| {
            self.refusal(&format!(
                "{flag} `{}` (explicit sandbox roots must sit inside sandbox.allowed_roots while max_profile is set)",
                crate::ui::events::sanitize_for_review(&root.display().to_string())
            ))
        })
    }

    /// True when `--dangerously-skip-permissions` must be refused: either the
    /// file says so, or it sets deny rules or mode limits, which only a live
    /// permission checker can enforce (skipping checks would void them).
    /// Refusal when `--sandbox-profile` asks for more than `max_profile`.
    pub(crate) fn profile_violation(
        &self,
        profile: crate::sandbox::SandboxProfile,
    ) -> Option<String> {
        let max = self.file.sandbox.max_profile?;
        (profile.rank() > max.rank()).then(|| {
            self.refusal(&format!(
                "sandbox profile `{profile}` (widest permitted: {max})"
            ))
        })
    }

    /// True when `sandbox.allowed_hosts` pins the network host list.
    pub(crate) fn network_hosts_pinned(&self) -> bool {
        self.file.sandbox.allowed_hosts.is_some()
    }

    /// Whether a user host entry stays inside the managed `allowed_hosts` pin.
    fn host_entry_allowed(&self, entry: &str) -> bool {
        let Some(pinned) = &self.file.sandbox.allowed_hosts else {
            return true;
        };
        let Ok(rule) = crate::sandbox::net_proxy::HostRule::parse(entry) else {
            return false;
        };
        pinned.iter().any(|pin| {
            crate::sandbox::net_proxy::HostRule::parse(pin).is_ok_and(|pin| rule.covered_by(&pin))
        })
    }

    pub(crate) fn permission_checks_required(&self) -> bool {
        let permissions = &self.file.permissions;
        permissions.require_permission_checks
            || permissions
                .deny
                .values()
                .any(|patterns| !patterns.is_empty())
            || permissions.allowed_modes.is_some()
            || permissions.disable_yolo
    }

    pub(crate) fn provider_allowed(&self, name: &str) -> bool {
        let providers = &self.file.providers;
        !providers.denied.iter().any(|denied| denied == name)
            && providers
                .allowed
                .as_ref()
                .is_none_or(|allowed| allowed.iter().any(|item| item == name))
    }

    pub(crate) fn check_provider(&self, name: &str) -> Result<(), String> {
        if self.provider_allowed(name) {
            Ok(())
        } else {
            Err(self.refusal(&format!(
                "provider `{}`",
                crate::ui::events::sanitize_for_review(name)
            )))
        }
    }

    /// Whether server `name`, defined as `config`, may run. Bare-name entries
    /// match the name only; table entries also pin `command`/`args` or `url`.
    #[cfg(feature = "mcp")]
    pub(crate) fn mcp_allowed(
        &self,
        name: &str,
        config: &crate::extras::mcp::config::McpServerConfig,
    ) -> bool {
        let mcp = &self.file.mcp;
        !mcp.disabled
            && !mcp
                .denied_servers
                .iter()
                .any(|denied| denied.matches(name, config, false))
            && mcp
                .allowed_servers
                .as_ref()
                .is_none_or(|allowed| allowed.iter().any(|item| item.matches(name, config, true)))
    }

    #[cfg(feature = "mcp")]
    pub(crate) fn check_mcp_server(
        &self,
        name: &str,
        config: &crate::extras::mcp::config::McpServerConfig,
    ) -> Result<(), String> {
        if self.mcp_allowed(name, config) {
            Ok(())
        } else {
            Err(self.refusal(&format!(
                "MCP server `{}`",
                crate::ui::events::sanitize_for_review(name)
            )))
        }
    }

    fn sandbox_pins_summary(&self) -> String {
        let sandbox = &self.file.sandbox;
        let mut pins = Vec::new();
        if let Some(profile) = sandbox.max_profile {
            pins.push(format!("max profile {profile}"));
        }
        if let Some(network) = sandbox.max_network {
            pins.push(format!("max network {network}"));
        }
        if let Some(hosts) = &sandbox.allowed_hosts {
            pins.push(format!("{} allowed host(s)", hosts.len()));
        }
        if sandbox.forbid_home_mounts {
            pins.push("no home mounts".to_string());
        }
        if let Some(roots) = &sandbox.allowed_roots {
            pins.push(format!("{} allowed root(s)", roots.len()));
        }
        if pins.is_empty() {
            "none".to_string()
        } else {
            pins.join(", ")
        }
    }

    /// One-line summary for `/doctor`; patterns and names are omitted.
    pub(crate) fn summary(&self) -> String {
        let deny: usize = self
            .file
            .permissions
            .deny
            .values()
            .map(|p| p.len().max(1))
            .sum();
        format!(
            "modes: {}; deny rules: {deny}; sandbox required: {}; permission checks required: {}; sandbox pins: {}; mcp: {}; providers: {}",
            self.allowed_modes
                .iter()
                .map(|mode| mode.as_str())
                .collect::<Vec<_>>()
                .join(","),
            if self.file.sandbox.required {
                "true".to_string()
            } else if self.sandbox_pinned() {
                "true (implied by sandbox pins)".to_string()
            } else {
                "false".to_string()
            },
            self.permission_checks_required(),
            self.sandbox_pins_summary(),
            if self.file.mcp.disabled {
                "disabled".to_string()
            } else if self.file.mcp.allowed_servers.is_some() {
                "allowlist".to_string()
            } else {
                "unrestricted".to_string()
            },
            if self.file.providers.allowed.is_some() {
                "allowlist"
            } else {
                "unrestricted"
            },
        )
    }

    /// Drop explicit read/write/toolchain/Windows roots outside the allowance.
    fn clamp_roots(&self, cfg: &mut super::Config) {
        let file = self.path.display();
        let mut dropped = Vec::new();
        for (key, roots) in [
            ("sandbox-read-roots", &mut cfg.sandbox_read_roots),
            ("sandbox-write-roots", &mut cfg.sandbox_write_roots),
            (
                "windows-appcontainer-read-roots",
                &mut cfg.windows_appcontainer_read_roots,
            ),
            (
                "windows-appcontainer-write-roots",
                &mut cfg.windows_appcontainer_write_roots,
            ),
        ] {
            let before = roots.len();
            roots.retain(|root| self.sandbox_root_allowed(root));
            if roots.len() != before {
                dropped.push(key);
            }
        }
        if let Some(roots) = cfg.sandbox_toolchain_roots.as_mut() {
            let before = roots.len();
            roots.retain(|root| self.sandbox_root_allowed(root));
            if roots.len() != before {
                dropped.push("sandbox-toolchain-roots");
                // Everything removed: fall back to the built-in defaults
                // rather than an empty list the user never asked for.
                if roots.is_empty() {
                    cfg.sandbox_toolchain_roots = None;
                }
            }
        }
        for key in dropped {
            cfg.load_warnings.push(format!(
                "{key} entries outside sandbox.allowed_roots were removed by the managed policy {file} (max_profile is set)"
            ));
        }
    }

    /// Clamp the sandbox settings of `cfg` to the managed pins, warning for
    /// each value that was lowered or dropped.
    fn apply_sandbox_pins(&self, cfg: &mut super::Config) {
        let sandbox = &self.file.sandbox;
        let file = self.path.display();
        if let Some(max) = sandbox.max_profile
            && cfg.sandbox_profile.is_some_and(|p| p.rank() > max.rank())
        {
            cfg.sandbox_profile = Some(max);
            cfg.load_warnings.push(format!(
                "sandbox-profile was lowered to `{max}` by the managed policy {file}"
            ));
        }
        // A host pin is meaningless under `allow`: it caps the network at
        // `allowlist` even when no `max_network` is set.
        let max_network = match (sandbox.max_network, sandbox.allowed_hosts.is_some()) {
            (Some(max), true) if max.rank() > crate::sandbox::SandboxNetwork::Allowlist.rank() => {
                Some(crate::sandbox::SandboxNetwork::Allowlist)
            }
            (None, true) => Some(crate::sandbox::SandboxNetwork::Allowlist),
            (max, _) => max,
        };
        if let Some(max) = max_network
            && cfg.sandbox_network.is_some_and(|n| n.rank() > max.rank())
        {
            cfg.sandbox_network = Some(max);
            cfg.load_warnings.push(format!(
                "sandbox-network was lowered to `{max}` by the managed policy {file}"
            ));
        }
        if sandbox.allowed_hosts.is_some() {
            let before = cfg.sandbox_network_hosts.len();
            cfg.sandbox_network_hosts
                .retain(|entry| self.host_entry_allowed(entry));
            if cfg.sandbox_network_hosts.len() != before {
                cfg.load_warnings.push(format!(
                    "sandbox-network-hosts entries outside the managed allowed_hosts were removed by the managed policy {file}"
                ));
            }
        }
        if sandbox.max_profile.is_some() {
            self.clamp_roots(cfg);
        }
        if sandbox.forbid_home_mounts && !cfg.sandbox_home_mounts.is_empty() {
            cfg.sandbox_home_mounts.clear();
            cfg.load_warnings.push(format!(
                "sandbox-home-mounts was cleared by the managed policy {file}"
            ));
        }
    }

    /// Merge the policy into `cfg` after every other layer: the result is what
    /// the rest of the program sees, so nothing earlier can override it.
    pub(crate) fn apply(&self, cfg: &mut super::Config) {
        let deny = cfg.permission_deny.get_or_insert_with(Default::default);
        for (tool, patterns) in &self.file.permissions.deny {
            // The file-modifying tools are separate rule keys; a managed deny
            // for one must not be bypassable through a sibling.
            let keys: &[&str] = if WRITE_FAMILY.contains(&tool.as_str()) {
                &WRITE_FAMILY
            } else {
                &[]
            };
            for key in std::iter::once(tool.as_str()).chain(keys.iter().copied()) {
                let entry = deny.entry(key.to_string()).or_default();
                for pattern in patterns {
                    // A shell command is one opaque string whose arguments are
                    // paths and URLs: `*` must cross `/`, or the documented
                    // `rm -rf *` would miss `rm -rf build/out`.
                    let pattern = if matches!(tool.as_str(), "shell" | "bash") {
                        crate::permission::checker::fetch_deny_glob(pattern)
                    } else {
                        pattern.clone()
                    };
                    if !entry.contains(&pattern) {
                        entry.push(pattern);
                    }
                }
            }
        }
        if self.sandbox_required() {
            cfg.sandbox = Some(true);
            cfg.sandbox_required = Some(true);
        }
        self.apply_sandbox_pins(cfg);
        if self.permission_checks_required() {
            cfg.permission_checks_required = Some(true);
        }
        if !self.mode_allowed(SecurityMode::Yolo) {
            cfg.yolo = Some(false);
        }
        #[cfg(feature = "mcp")]
        if let Some(servers) = cfg.mcp_servers.as_mut() {
            let removed: Vec<String> = servers
                .iter()
                .filter(|(name, config)| !self.mcp_allowed(name, config))
                .map(|(name, _)| name.clone())
                .collect();
            for name in removed {
                servers.remove(&name);
                cfg.load_warnings.push(format!(
                    "MCP server `{}` was removed by the managed policy {}",
                    crate::ui::events::sanitize_for_review(&name),
                    self.path.display()
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn policy(text: &str) -> ManagedPolicy {
        ManagedPolicy::from_file(
            PathBuf::from("/etc/mini-agent/managed-config.toml"),
            toml::from_str(text).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn modes_are_clamped_to_the_nearest_permitted_one() {
        let policy = policy("[permissions]\ndisable_yolo = true\n");
        assert!(policy.mode_allowed(SecurityMode::Standard));
        assert!(!policy.mode_allowed(SecurityMode::Yolo));
        assert_eq!(
            policy.clamp_mode(SecurityMode::Yolo),
            SecurityMode::Standard
        );
        let narrow = super::ManagedPolicy::from_file(
            PathBuf::from("/m.toml"),
            toml::from_str("[permissions]\nallowed_modes = [\"readonly\", \"guarded\"]\n").unwrap(),
        )
        .unwrap();
        assert_eq!(
            narrow.clamp_mode(SecurityMode::Standard),
            SecurityMode::Guarded
        );
        assert_eq!(
            narrow.clamp_mode(SecurityMode::PlanWrite),
            SecurityMode::ReadOnly
        );
        let message = narrow.mode_violation(SecurityMode::Yolo).unwrap();
        assert!(
            message.contains("/m.toml") && message.contains("yolo"),
            "{message}"
        );
    }

    #[test]
    fn empty_mode_list_and_unknown_keys_are_rejected() {
        let empty = ManagedPolicy::from_file(
            PathBuf::from("/m.toml"),
            toml::from_str("[permissions]\nallowed_modes = [\"yolo\"]\ndisable_yolo = true\n")
                .unwrap(),
        );
        assert!(empty.is_err());
        assert!(toml::from_str::<ManagedFile>("[permisions]\nx = 1\n").is_err());
        let bad_mode = ManagedPolicy::from_file(
            PathBuf::from("/m.toml"),
            toml::from_str("[permissions]\nallowed_modes = [\"nope\"]\n").unwrap(),
        );
        assert!(bad_mode.is_err());
    }

    #[test]
    fn apply_wins_over_user_and_project_values() {
        let policy = policy(
            "[permissions]\ndeny = { shell = [\"rm -rf *\"] }\ndisable_yolo = true\nrequire_permission_checks = true\n[sandbox]\nrequired = true\n",
        );
        let mut cfg: super::super::Config = toml::from_str(
            "yolo = true\nsandbox = false\nsandbox_required = false\npermission_deny = { shell = [\"curl *\"] }\n",
        )
        .unwrap();
        policy.apply(&mut cfg);
        assert_eq!(cfg.yolo, Some(false));
        assert_eq!(cfg.sandbox, Some(true));
        assert_eq!(cfg.sandbox_required, Some(true));
        assert_eq!(cfg.permission_checks_required, Some(true));
        assert_eq!(
            cfg.permission_deny.as_ref().unwrap()["shell"],
            ["curl *", "rm -rf **"]
        );
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn apply_drops_mcp_servers_outside_the_allowlist() {
        let policy = policy("[mcp]\nallowed_servers = [\"ok\"]\n");
        let mut cfg: super::super::Config = toml::from_str(
            "[mcp_servers.ok]\ncommand = \"a\"\n[mcp_servers.evil]\ncommand = \"b\"\n",
        )
        .unwrap();
        policy.apply(&mut cfg);
        let servers = cfg.mcp_servers.as_ref().unwrap();
        assert!(servers.contains_key("ok") && !servers.contains_key("evil"));
        assert!(
            cfg.load_warnings
                .iter()
                .any(|w| w.contains("managed-config.toml"))
        );
    }

    #[test]
    fn provider_and_mcp_lists_deny_over_allow() {
        let policy = policy(
            "[providers]\nallowed = [\"anthropic\", \"openai\"]\ndenied = [\"openai\"]\n[mcp]\ndenied_servers = [\"x\"]\n",
        );
        assert!(policy.check_provider("anthropic").is_ok());
        assert!(
            policy
                .check_provider("openai")
                .unwrap_err()
                .contains("managed-config.toml")
        );
        assert!(policy.check_provider("ollama").is_err());
    }

    #[cfg(feature = "mcp")]
    fn server(text: &str) -> crate::extras::mcp::config::McpServerConfig {
        toml::from_str(text).unwrap()
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_lists_deny_over_allow_and_disable_everything() {
        let any = server("command = \"a\"\n");
        let policy = policy("[mcp]\ndenied_servers = [\"x\"]\n");
        assert!(policy.check_mcp_server("y", &any).is_ok());
        assert!(policy.check_mcp_server("x", &any).is_err());
        let off = super::ManagedPolicy::from_file(
            PathBuf::from("/m.toml"),
            toml::from_str("[mcp]\ndisabled = true\n").unwrap(),
        )
        .unwrap();
        assert!(off.check_mcp_server("anything", &any).is_err());
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_entries_pin_what_runs_not_just_the_name() {
        let policy = policy(
            "[mcp]\nallowed_servers = [\"plain\", { name = \"github\", command = \"npx\", args = [\"-y\", \"gh-mcp\"] }, { name = \"docs\", url = \"https://docs.example/mcp\" }]\n",
        );
        let ok = |name: &str, text: &str| policy.check_mcp_server(name, &server(text));
        // Name-only entries keep matching by name alone.
        assert!(ok("plain", "command = \"anything\"\n").is_ok());
        assert!(ok("plain", "url = \"https://evil.example\"\n").is_ok());
        // Pinned command and args.
        assert!(ok("github", "command = \"npx\"\nargs = [\"-y\", \"gh-mcp\"]\n").is_ok());
        let swapped = ok("github", "command = \"sh\"\nargs = [\"-c\", \"evil\"]\n").unwrap_err();
        assert!(swapped.contains("managed-config.toml"), "{swapped}");
        assert!(ok("github", "command = \"npx\"\nargs = [\"-y\", \"evil\"]\n").is_err());
        assert!(
            ok(
                "github",
                "command = \"npx\"\nargs = [\"-y\", \"gh-mcp\", \"--x\"]\n"
            )
            .is_err()
        );
        assert!(ok("github", "url = \"https://github.example/mcp\"\n").is_err());
        // Pinned url, trailing slash ignored; other command or url refused.
        assert!(ok("docs", "url = \"https://docs.example/mcp/\"\n").is_ok());
        assert!(ok("docs", "url = \"https://other.example/mcp\"\n").is_err());
        assert!(ok("docs", "command = \"sh\"\n").is_err());
        // Unlisted names stay refused.
        assert!(ok("other", "command = \"npx\"\n").is_err());

        // A pinned command with no args allows only an argument-free launch.
        let bare = super::ManagedPolicy::from_file(
            PathBuf::from("/m.toml"),
            toml::from_str("[mcp]\nallowed_servers = [{ command = \"srv\" }]\n").unwrap(),
        )
        .unwrap();
        assert!(
            bare.check_mcp_server("any", &server("command = \"srv\"\n"))
                .is_ok()
        );
        assert!(
            bare.check_mcp_server("any", &server("command = \"srv\"\nargs = [\"--x\"]\n"))
                .is_err()
        );
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_deny_entries_match_by_identity_and_apply_drops_mismatches() {
        let policy = policy(
            "[mcp]\ndenied_servers = [{ command = \"sh\" }, { url = \"https://bad.example\" }]\nallowed_servers = [{ name = \"ok\", command = \"a\" }]\n",
        );
        // Deny entries ignore args when none are pinned and ignore the name.
        assert!(policy.mcp_allowed("ok", &server("command = \"a\"\n")));
        assert!(!policy.mcp_allowed("ok", &server("command = \"sh\"\nargs = [\"-c\", \"x\"]\n")));
        assert!(!policy.mcp_allowed("ok", &server("url = \"https://bad.example/\"\n")));
        let mut cfg: super::super::Config = toml::from_str(
            "[mcp_servers.ok]\ncommand = \"a\"\n[mcp_servers.fake]\ncommand = \"a\"\n",
        )
        .unwrap();
        let mut spoof: super::super::Config =
            toml::from_str("[mcp_servers.ok]\ncommand = \"b\"\n").unwrap();
        policy.apply(&mut cfg);
        policy.apply(&mut spoof);
        assert!(cfg.mcp_servers.as_ref().unwrap().contains_key("ok"));
        assert!(!cfg.mcp_servers.as_ref().unwrap().contains_key("fake"));
        assert!(!spoof.mcp_servers.as_ref().unwrap().contains_key("ok"));
    }

    #[test]
    fn invalid_mcp_entries_are_rejected() {
        for bad in [
            "[mcp]\nallowed_servers = [{}]\n",
            "[mcp]\nallowed_servers = [{ command = \"a\", url = \"https://x\" }]\n",
            "[mcp]\nallowed_servers = [{ name = \"a\", args = [\"x\"] }]\n",
        ] {
            let parsed =
                ManagedPolicy::from_file(PathBuf::from("/m.toml"), toml::from_str(bad).unwrap());
            assert!(parsed.is_err(), "{bad}");
        }
        assert!(
            toml::from_str::<ManagedFile>("[mcp]\nallowed_servers = [{ nme = \"a\" }]\n").is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn insecure_or_invalid_files_fail_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::tests::support::temp::TempDir::new("managed-policy");
        let path = dir.path().join("managed-config.toml");
        assert!(ManagedPolicy::load(&path, false).unwrap().is_none());

        std::fs::write(&path, "[sandbox]\nrequired = true\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let loaded = ManagedPolicy::load(&path, false).unwrap().unwrap();
        assert!(loaded.sandbox_required());
        // The test user is not root: the production owner check refuses it.
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&path).unwrap().uid() != 0 {
            assert!(
                ManagedPolicy::load(&path, true)
                    .unwrap_err()
                    .contains("owned by root")
            );
        }

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(
            ManagedPolicy::load(&path, false)
                .unwrap_err()
                .contains("writable")
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        std::fs::write(&path, "[sandbox\n").unwrap();
        assert!(
            ManagedPolicy::load(&path, false)
                .unwrap_err()
                .contains("invalid")
        );

        let link = dir.path().join("link.toml");
        std::fs::write(dir.path().join("real.toml"), "").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.toml"), &link).unwrap();
        assert!(
            ManagedPolicy::load(&link, false)
                .unwrap_err()
                .contains("regular file")
        );
    }

    fn authority(
        policy: &ManagedPolicy,
        args: &[&str],
        cfg: &super::super::Config,
    ) -> Result<crate::permission::ResolvedExecutionAuthority, String> {
        let cli = crate::cli::Cli::try_parse_from(
            std::iter::once("mini-agent").chain(args.iter().copied()),
        )
        .unwrap();
        crate::permission::resolve_execution_authority_under(
            Some(policy),
            &cli,
            cfg,
            crate::sandbox::SandboxPolicy::Disabled,
            "disabled",
        )
        .map_err(|error| error.to_string())
    }

    #[test]
    fn flags_and_project_config_that_widen_authority_are_refused_naming_the_file() {
        let policy = policy(
            "[permissions]\ndisable_yolo = true\nrequire_permission_checks = true\n[sandbox]\nrequired = true\n[providers]\nallowed = [\"anthropic\"]\n",
        );
        let cfg: super::super::Config = toml::from_str("provider = \"anthropic\"\n").unwrap();
        assert!(authority(&policy, &[], &cfg).is_ok());
        for args in [
            &["--yolo"][..],
            &["--dangerously-skip-permissions"][..],
            &["--no-sandbox"][..],
            &["--provider", "openai"][..],
        ] {
            let error = authority(&policy, args, &cfg).unwrap_err();
            assert!(error.contains("managed-config.toml"), "{args:?}: {error}");
            assert!(!error.contains("user configuration"), "{error}");
        }
        // A project/user config that selects yolo or another provider is refused too.
        for text in [
            "yolo = true\nprovider = \"anthropic\"\n",
            "provider = \"openai\"\n",
        ] {
            let cfg: super::super::Config = toml::from_str(text).unwrap();
            let error = authority(&policy, &[], &cfg).unwrap_err();
            assert!(error.contains("managed-config.toml"), "{text}: {error}");
        }
        let cfg: super::super::Config =
            toml::from_str("default_permission_mode = \"yolo\"\nprovider = \"anthropic\"\n")
                .unwrap();
        assert!(
            authority(&policy, &[], &cfg)
                .unwrap_err()
                .contains("managed-config.toml")
        );
    }

    #[test]
    fn deny_rules_and_mode_limits_imply_refusing_skip_permissions() {
        let cfg: super::super::Config = toml::from_str("").unwrap();
        for text in [
            "[permissions]\ndeny = { shell = [\"rm -rf *\"] }\n",
            "[permissions]\ndisable_yolo = true\n",
            "[permissions]\nallowed_modes = [\"standard\"]\n",
        ] {
            let policy = policy(text);
            assert!(policy.permission_checks_required(), "{text}");
            let error = authority(&policy, &["--dangerously-skip-permissions"], &cfg).unwrap_err();
            assert!(error.contains("managed-config.toml"), "{text}: {error}");
            assert!(authority(&policy, &[], &cfg).is_ok(), "{text}");
        }
        // Policies without permission rules keep the flag usable.
        let unrelated = policy("[sandbox]\nrequired = true\n");
        assert!(!unrelated.permission_checks_required());
        // An empty deny table does not count as a rule.
        let empty = policy("[permissions]\ndeny = { shell = [] }\n");
        assert!(!empty.permission_checks_required());
        let mut applied: super::super::Config = toml::from_str("").unwrap();
        policy("[permissions]\ndisable_yolo = true\n").apply(&mut applied);
        assert_eq!(applied.permission_checks_required, Some(true));
    }

    #[test]
    fn sandbox_pins_refuse_flags_and_clamp_every_config_layer() {
        let policy = policy(
            "[sandbox]\nrequired = true\nmax_profile = \"workspace\"\nmax_network = \"allowlist\"\nallowed_hosts = [\"registry.npmjs.org\", \"*.github.com\"]\nforbid_home_mounts = true\n",
        );
        let cfg: super::super::Config = toml::from_str("").unwrap();
        let error = authority(&policy, &["--sandbox-profile", "host-full"], &cfg).unwrap_err();
        assert!(
            error.contains("managed-config.toml") && error.contains("host-full"),
            "{error}"
        );
        assert!(authority(&policy, &["--sandbox-profile", "workspace"], &cfg).is_ok());
        assert!(authority(&policy, &[], &cfg).is_ok());

        // User/project config that widens everything is clamped by apply().
        let mut cfg: super::super::Config = toml::from_str(
            "sandbox_profile = \"host-full\"\nsandbox_network = \"allow\"\nsandbox_home_mounts = [\".aws:rw\"]\nsandbox_network_hosts = [\"registry.npmjs.org\", \"evil.example.com\", \"*.github.com\", \"api.github.com\", \"npmjs.org\", \"registry.npmjs.org:8080\", \"*.org\"]\n",
        )
        .unwrap();
        policy.apply(&mut cfg);
        assert_eq!(
            cfg.sandbox_profile,
            Some(crate::sandbox::SandboxProfile::Workspace)
        );
        assert_eq!(
            cfg.sandbox_network,
            Some(crate::sandbox::SandboxNetwork::Allowlist)
        );
        assert!(cfg.sandbox_home_mounts.is_empty());
        assert_eq!(
            cfg.sandbox_network_hosts,
            ["registry.npmjs.org", "*.github.com", "api.github.com"]
        );
        assert!(
            cfg.load_warnings
                .iter()
                .all(|w| w.contains("managed-config.toml"))
        );
        assert_eq!(cfg.load_warnings.len(), 4);
        assert!(policy.network_hosts_pinned());
        let summary = policy.summary();
        assert!(
            summary.contains("max profile workspace")
                && summary.contains("max network allowlist")
                && summary.contains("2 allowed host(s)")
                && summary.contains("no home mounts"),
            "{summary}"
        );
    }

    #[test]
    fn sandbox_pins_are_validated_and_absent_pins_change_nothing() {
        for bad in [
            "[sandbox]\nmax_profile = \"root\"\n",
            "[sandbox]\nmax_network = \"open\"\n",
        ] {
            assert!(toml::from_str::<ManagedFile>(bad).is_err(), "{bad}");
        }
        let invalid_host = ManagedPolicy::from_file(
            PathBuf::from("/m.toml"),
            toml::from_str("[sandbox]\nallowed_hosts = [\"https://x.example\"]\n").unwrap(),
        );
        assert!(invalid_host.is_err());
        let open = policy("[sandbox]\nrequired = true\n");
        let mut cfg: super::super::Config = toml::from_str(
            "sandbox_profile = \"host-full\"\nsandbox_network = \"allow\"\nsandbox_home_mounts = [\".aws:rw\"]\nsandbox_network_hosts = [\"x.example.com\"]\n",
        )
        .unwrap();
        open.apply(&mut cfg);
        assert_eq!(
            cfg.sandbox_profile,
            Some(crate::sandbox::SandboxProfile::HostFull)
        );
        assert_eq!(cfg.sandbox_home_mounts.len(), 1);
        assert_eq!(cfg.sandbox_network_hosts.len(), 1);
        assert!(!open.network_hosts_pinned());
        assert!(open.summary().contains("sandbox pins: none"));
    }

    #[test]
    fn managed_deny_beats_a_user_allow_rule() {
        let policy = policy("[permissions]\ndeny = { shell = [\"npm publish*\"] }\n");
        let mut cfg: super::super::Config =
            toml::from_str("permission_allow = { shell = [\"npm publish*\", \"npm test\"] }\n")
                .unwrap();
        policy.apply(&mut cfg);
        let configs = cfg.build_permission_config().unwrap();
        let mut checker = crate::permission::checker::PermissionChecker::new(
            &configs,
            SecurityMode::Standard,
            std::env::current_dir().ok(),
            None,
        )
        .unwrap();
        assert!(matches!(
            checker.check("shell", "npm publish --tag x"),
            crate::permission::checker::CheckResult::Denied(_)
        ));
        assert!(matches!(
            checker.check("shell", "npm test"),
            crate::permission::checker::CheckResult::Allowed
        ));
    }

    // mini-agent-iknn9: the documented `rm -rf *` example must deny real paths.
    #[test]
    fn managed_shell_deny_star_crosses_path_separators() {
        let policy = policy("[permissions]\ndeny = { shell = [\"rm -rf *\", \"curl *\"] }\n");
        let mut cfg: super::super::Config = toml::from_str("").unwrap();
        policy.apply(&mut cfg);
        let configs = cfg.build_permission_config().unwrap();
        let mut checker = crate::permission::checker::PermissionChecker::new(
            &configs,
            SecurityMode::Standard,
            std::env::current_dir().ok(),
            None,
        )
        .unwrap();
        for script in [
            "rm -rf build/out",
            "rm -rf ./a/b",
            "rm -rf /home/u/proj",
            "curl https://x/y",
        ] {
            assert!(
                matches!(
                    checker.check("shell", script),
                    crate::permission::checker::CheckResult::Denied(_)
                ),
                "{script}"
            );
        }
    }

    #[test]
    fn managed_write_deny_covers_every_file_modifying_tool() {
        for key in ["write", "edit", "notebook_edit"] {
            let policy = policy(&format!(
                "[permissions]\ndeny = {{ {key} = [\"/etc/**\"] }}\n"
            ));
            let mut cfg: super::super::Config = toml::from_str(
                "permission_allow = { edit = [\"/etc/**\"], write = [\"/etc/**\"] }\n",
            )
            .unwrap();
            policy.apply(&mut cfg);
            let configs = cfg.build_permission_config().unwrap();
            let checker = crate::permission::checker::PermissionChecker::new(
                &configs,
                SecurityMode::Yolo,
                std::env::current_dir().ok(),
                None,
            )
            .unwrap();
            for tool in ["write", "edit", "notebook_edit", "js/write_file"] {
                assert!(
                    checker.path_is_denied_lexically(tool, "/etc/hosts"),
                    "{key} deny must cover {tool}"
                );
            }
            assert!(!checker.path_is_denied_lexically("read", "/etc/hosts"));
        }
    }

    #[test]
    fn runtime_profile_switch_refuses_above_max_profile_naming_the_file() {
        let policy = policy("[sandbox]\nmax_profile = \"workspace\"\n");
        let mut sandbox = crate::sandbox::Sandbox::new(false, "bwrap");
        for profile in [
            crate::sandbox::SandboxProfile::HostRead,
            crate::sandbox::SandboxProfile::HostFull,
        ] {
            let error = sandbox
                .set_profile_under(Some(&policy), profile)
                .unwrap_err();
            assert!(error.contains("managed-config.toml"), "{error}");
            assert_eq!(sandbox.profile(), crate::sandbox::SandboxProfile::Workspace);
        }
        // Default closed: without a policy the managed check never fires.
        let unmanaged = sandbox.set_profile_under(None, crate::sandbox::SandboxProfile::HostRead);
        assert!(
            unmanaged
                .err()
                .is_none_or(|e| !e.contains("managed-config.toml"))
        );
    }

    #[test]
    fn max_profile_clears_roots_outside_the_allowance() {
        let dir = crate::tests::support::temp::TempDir::new("managed-roots");
        let allowed = dir.path().join("tools");
        std::fs::create_dir_all(&allowed).unwrap();
        let outside = crate::tests::support::temp::TempDir::new("managed-outside");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), allowed.join("escape")).unwrap();
        let pinned = policy(&format!(
            "[sandbox]\nmax_profile = \"workspace\"\nallowed_roots = [{:?}]\n",
            allowed.display().to_string()
        ));
        let mut cfg: super::super::Config = toml::from_str(&format!(
            "sandbox_write_roots = [\"/etc\", {ok:?}, {sneak:?}]\nsandbox_read_roots = [\"/home/x\", \"relative\"]\nsandbox_toolchain_roots = [\"/opt/evil\"]\nwindows_appcontainer_write_roots = [\"C:/x\"]\n",
            ok = allowed.join("sub").display().to_string(),
            sneak = allowed.join("../outside").display().to_string(),
        ))
        .unwrap();
        pinned.apply(&mut cfg);
        assert_eq!(cfg.sandbox_write_roots, vec![allowed.join("sub")]);
        assert!(cfg.sandbox_read_roots.is_empty());
        assert_eq!(cfg.sandbox_toolchain_roots, None);
        assert!(cfg.windows_appcontainer_write_roots.is_empty());
        #[cfg(unix)]
        assert!(!pinned.sandbox_root_allowed(&allowed.join("escape")));
        assert!(
            cfg.load_warnings
                .iter()
                .any(|w| w.contains("sandbox-write-roots") && w.contains("managed-config.toml"))
        );

        // No allowance key: every explicit root is cleared.
        let strict = policy("[sandbox]\nmax_profile = \"host-read\"\n");
        let mut cfg: super::super::Config =
            toml::from_str("sandbox_write_roots = [\"/tmp/x\"]\n").unwrap();
        strict.apply(&mut cfg);
        assert!(cfg.sandbox_write_roots.is_empty());
        // No max_profile: roots are untouched (default closed for the new key).
        let open = policy("[sandbox]\nrequired = true\n");
        let mut cfg: super::super::Config =
            toml::from_str("sandbox_write_roots = [\"/tmp/x\"]\n").unwrap();
        open.apply(&mut cfg);
        assert_eq!(cfg.sandbox_write_roots.len(), 1);
        // CLI toolchain roots are refused naming the file.
        let cli_cfg: super::super::Config = toml::from_str("").unwrap();
        let error = authority(
            &strict,
            &["--sandbox-toolchain-root", "/opt/evil"],
            &cli_cfg,
        )
        .unwrap_err();
        assert!(error.contains("--sandbox-toolchain-root"), "{error}");
        assert!(error.contains("managed-config.toml"), "{error}");
    }

    #[test]
    fn allowed_roots_need_max_profile_and_absolute_paths() {
        for bad in [
            "[sandbox]\nallowed_roots = [\"/opt\"]\n",
            "[sandbox]\nmax_profile = \"workspace\"\nallowed_roots = [\"rel\"]\n",
        ] {
            assert!(
                ManagedPolicy::from_file(PathBuf::from("/m.toml"), toml::from_str(bad).unwrap())
                    .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn allowed_hosts_caps_network_at_allowlist() {
        let policy = policy("[sandbox]\nallowed_hosts = [\"registry.npmjs.org\"]\n");
        let mut cfg: super::super::Config =
            toml::from_str("sandbox_network = \"allow\"\n").unwrap();
        policy.apply(&mut cfg);
        assert_eq!(
            cfg.sandbox_network,
            Some(crate::sandbox::SandboxNetwork::Allowlist)
        );
    }

    #[test]
    fn any_sandbox_pin_implies_required_and_refuses_no_sandbox() {
        for pin in [
            "max_profile = \"workspace\"",
            "max_network = \"deny\"",
            "allowed_hosts = [\"a.example\"]",
            "forbid_home_mounts = true",
        ] {
            let policy = policy(&format!("[sandbox]\n{pin}\n"));
            assert!(policy.sandbox_required(), "{pin}");
            assert!(policy.summary().contains("implied by sandbox pins"));
            let cfg: super::super::Config = toml::from_str("").unwrap();
            let error = authority(&policy, &["--no-sandbox"], &cfg).unwrap_err();
            assert!(error.contains("--no-sandbox"), "{error}");
            assert!(error.contains("managed-config.toml"), "{error}");
            let off: super::super::Config = toml::from_str("sandbox = false\n").unwrap();
            assert!(authority(&policy, &[], &off).is_err(), "{pin}");
            let mut applied: super::super::Config = toml::from_str("sandbox = false\n").unwrap();
            policy.apply(&mut applied);
            assert_eq!(applied.sandbox, Some(true));
            assert_eq!(applied.sandbox_required, Some(true));
        }
        // No pins: nothing is implied (default closed).
        let none = policy("[permissions]\ndisable_yolo = true\n");
        assert!(!none.sandbox_required());
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn pinned_mcp_launch_rejects_env_cwd_inherit_env_and_unpinned_sandbox() {
        let policy = policy(
            "[mcp]\nallowed_servers = [{ name = \"gh\", command = \"npx\", args = [\"-y\", \"gh-mcp\"] }, { name = \"box\", command = \"srv\", sandbox = \"bwrap\", env = { A = \"1\" } }, { name = \"docs\", url = \"https://docs.example/mcp\" }, { name = \"hdr\", url = \"https://h.example\", headers = { X = \"y\" } }]\n",
        );
        let ok = |name: &str, text: &str| policy.check_mcp_server(name, &server(text));
        let base = "command = \"npx\"\nargs = [\"-y\", \"gh-mcp\"]\n";
        assert!(ok("gh", base).is_ok());
        for extra in [
            "env = { NODE_OPTIONS = \"--require /tmp/x.js\" }\n",
            "env = { LD_PRELOAD = \"/tmp/x.so\" }\n",
            "cwd = \"/tmp/planted\"\n",
            "inherit_env = [\"PATH\"]\n",
        ] {
            let error = ok("gh", &format!("{base}{extra}")).unwrap_err();
            assert!(error.contains("managed-config.toml"), "{extra}: {error}");
        }
        // Pinned sandbox and env: exact match required.
        assert!(
            ok(
                "box",
                "command = \"srv\"\nsandbox = \"bwrap\"\nenv = { A = \"1\" }\n"
            )
            .is_ok()
        );
        assert!(ok("box", "command = \"srv\"\nenv = { A = \"1\" }\n").is_err());
        assert!(
            ok(
                "box",
                "command = \"srv\"\nsandbox = \"none\"\nenv = { A = \"1\" }\n"
            )
            .is_err()
        );
        assert!(
            ok(
                "box",
                "command = \"srv\"\nsandbox = \"bwrap\"\nenv = { A = \"2\" }\n"
            )
            .is_err()
        );
        assert!(
            ok(
                "box",
                "command = \"srv\"\nsandbox = \"bwrap\"\nenv = { A = \"1\", B = \"2\" }\n"
            )
            .is_err()
        );
        // URL entries: headers_helper (a spawned command) is refused; pinned headers are exact.
        assert!(
            ok(
                "docs",
                "url = \"https://docs.example/mcp\"\nheaders = { Authorization = \"t\" }\n"
            )
            .is_ok()
        );
        assert!(
            ok(
                "docs",
                "url = \"https://docs.example/mcp\"\nheaders_helper = [\"/tmp/x\"]\n"
            )
            .is_err()
        );
        assert!(
            ok(
                "hdr",
                "url = \"https://h.example\"\nheaders = { X = \"y\" }\n"
            )
            .is_ok()
        );
        assert!(
            ok(
                "hdr",
                "url = \"https://h.example\"\nheaders = { X = \"z\" }\n"
            )
            .is_err()
        );
        // Deny entries still match on identity alone.
        let deny = ManagedPolicy::from_file(
            PathBuf::from("/m.toml"),
            toml::from_str("[mcp]\ndenied_servers = [{ command = \"sh\" }]\n").unwrap(),
        )
        .unwrap();
        assert!(
            deny.check_mcp_server("x", &server("command = \"sh\"\nenv = { A = \"1\" }\n"))
                .is_err()
        );
    }
}
