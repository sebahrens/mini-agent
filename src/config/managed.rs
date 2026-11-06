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
//!
//! [mcp]
//! disabled = false
//! allowed_servers = ["github"]                          # absent = no allowlist
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
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct McpPolicy {
    disabled: bool,
    allowed_servers: Option<Vec<String>>,
    denied_servers: Vec<String>,
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
pub(crate) fn check_mcp_server(name: &str) -> Result<(), String> {
    match active() {
        Some(policy) => policy.check_mcp_server(name),
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
            ("providers.allowed", file.providers.allowed.as_ref()),
            ("providers.denied", Some(&file.providers.denied)),
        ] {
            if let Some(list) = list {
                bounded_list(what, list).map_err(|e| format!("managed policy {shown}: {e}"))?;
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

    pub(crate) fn sandbox_required(&self) -> bool {
        self.file.sandbox.required
    }

    /// True when `--dangerously-skip-permissions` must be refused: either the
    /// file says so, or it sets deny rules or mode limits, which only a live
    /// permission checker can enforce (skipping checks would void them).
    pub(crate) fn permission_checks_required(&self) -> bool {
        let permissions = &self.file.permissions;
        permissions.require_permission_checks
            || permissions.deny.values().any(|patterns| !patterns.is_empty())
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

    #[cfg(feature = "mcp")]
    pub(crate) fn mcp_allowed(&self, name: &str) -> bool {
        let mcp = &self.file.mcp;
        !mcp.disabled
            && !mcp.denied_servers.iter().any(|denied| denied == name)
            && mcp
                .allowed_servers
                .as_ref()
                .is_none_or(|allowed| allowed.iter().any(|item| item == name))
    }

    #[cfg(feature = "mcp")]
    pub(crate) fn check_mcp_server(&self, name: &str) -> Result<(), String> {
        if self.mcp_allowed(name) {
            Ok(())
        } else {
            Err(self.refusal(&format!(
                "MCP server `{}`",
                crate::ui::events::sanitize_for_review(name)
            )))
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
            "modes: {}; deny rules: {deny}; sandbox required: {}; permission checks required: {}; mcp: {}; providers: {}",
            self.allowed_modes
                .iter()
                .map(|mode| mode.as_str())
                .collect::<Vec<_>>()
                .join(","),
            self.file.sandbox.required,
            self.permission_checks_required(),
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

    /// Merge the policy into `cfg` after every other layer: the result is what
    /// the rest of the program sees, so nothing earlier can override it.
    pub(crate) fn apply(&self, cfg: &mut super::Config) {
        let deny = cfg.permission_deny.get_or_insert_with(Default::default);
        for (tool, patterns) in &self.file.permissions.deny {
            let entry = deny.entry(tool.clone()).or_default();
            for pattern in patterns {
                if !entry.contains(pattern) {
                    entry.push(pattern.clone());
                }
            }
        }
        if self.file.sandbox.required {
            cfg.sandbox = Some(true);
            cfg.sandbox_required = Some(true);
        }
        if self.permission_checks_required() {
            cfg.permission_checks_required = Some(true);
        }
        if !self.mode_allowed(SecurityMode::Yolo) {
            cfg.yolo = Some(false);
        }
        #[cfg(feature = "mcp")]
        if let Some(servers) = cfg.mcp_servers.as_mut() {
            let removed: Vec<String> = servers
                .keys()
                .filter(|name| !self.mcp_allowed(name))
                .cloned()
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
            ["curl *", "rm -rf *"]
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
    #[test]
    fn mcp_lists_deny_over_allow_and_disable_everything() {
        let policy = policy("[mcp]\ndenied_servers = [\"x\"]\n");
        assert!(policy.check_mcp_server("y").is_ok());
        assert!(policy.check_mcp_server("x").is_err());
        let off = super::ManagedPolicy::from_file(
            PathBuf::from("/m.toml"),
            toml::from_str("[mcp]\ndisabled = true\n").unwrap(),
        )
        .unwrap();
        assert!(off.check_mcp_server("anything").is_err());
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
}
