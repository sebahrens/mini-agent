pub mod ask;
pub mod checker;
pub mod pattern;

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolPerm {
    Simple(Action),
    Granular(HashMap<String, Action>),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PermissionConfig {
    #[serde(rename = "*")]
    pub default: Option<Action>,
    #[serde(alias = "shell")]
    pub bash: Option<ToolPerm>,
    #[serde(rename = "git/status")]
    pub git_status: Option<ToolPerm>,
    #[serde(rename = "git/diff")]
    pub git_diff: Option<ToolPerm>,
    #[serde(rename = "git/log")]
    pub git_log: Option<ToolPerm>,
    #[serde(rename = "git/show")]
    pub git_show: Option<ToolPerm>,
    #[serde(rename = "git/stage")]
    pub git_stage: Option<ToolPerm>,
    #[serde(rename = "git/unstage")]
    pub git_unstage: Option<ToolPerm>,
    #[serde(rename = "git/commit")]
    pub git_commit: Option<ToolPerm>,
    #[serde(rename = "js/fetch", alias = "fetch")]
    pub js_fetch: Option<ToolPerm>,
    pub read: Option<ToolPerm>,
    pub write: Option<ToolPerm>,
    pub edit: Option<ToolPerm>,
    pub grep: Option<ToolPerm>,
    pub find_files: Option<ToolPerm>,
    pub list_dir: Option<ToolPerm>,
    #[serde(alias = "write_todo_list")]
    pub todo_write: Option<ToolPerm>,
    pub mcp_tool: Option<ToolPerm>,
    pub external_directory: Option<HashMap<String, Action>>,
    pub doom_loop: Option<Action>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_entries: Option<HashMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_entries: Option<HashMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny_entries: Option<HashMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionConfigs {
    pub glob: PermissionConfig,
    pub regex: PermissionConfig,
}

pub(crate) fn is_configurable_tool_name(tool: &str) -> bool {
    matches!(
        tool,
        "bash"
            | "shell"
            | "git/status"
            | "git/diff"
            | "git/log"
            | "git/show"
            | "git/stage"
            | "git/unstage"
            | "git/commit"
            | "js/fetch"
            | "fetch"
            | "read"
            | "write"
            | "edit"
            | "grep"
            | "find_files"
            | "list_dir"
            | "todo_write"
            | "write_todo_list"
            | "mcp_tool"
    )
}

impl From<PermissionConfig> for PermissionConfigs {
    fn from(glob: PermissionConfig) -> Self {
        PermissionConfigs {
            glob,
            regex: PermissionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SandboxResolution {
    Disabled,
    Enforced,
    DegradedUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ResolvedExecutionAuthority {
    pub mode: SecurityMode,
    pub tools_enabled: bool,
    pub permission_checks_enabled: bool,
    pub sandbox: SandboxResolution,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum ExecutionAuthorityError {
    #[error(
        "sandbox backend '{backend}' was not found — refusing to start with unsandboxed execution (use --no-sandbox to disable sandboxing explicitly)"
    )]
    SandboxUnavailable { backend: String },
    #[error(
        "invalid `default_permission_mode` value `{value}`: expected one of {accepted} (or `accept` as an alias for `standard`)"
    )]
    InvalidDefaultPermissionMode { value: String, accepted: String },
}

/// Resolve every user/config input that changes model execution authority.
///
/// The function is pure: callers supply the already-observed sandbox policy,
/// and both startup and ACP consume the same value. Approval-channel wiring
/// intentionally remains frontend-specific.
pub(crate) fn resolve_execution_authority(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
    sandbox_policy: crate::sandbox::SandboxPolicy,
    sandbox_backend: &str,
) -> Result<ResolvedExecutionAuthority, ExecutionAuthorityError> {
    // Validate the configured default before any flag can mask a typo: an
    // unknown mode name must never silently degrade to `standard`.
    let configured_default = match cfg.default_permission_mode.as_deref() {
        None => SecurityMode::Standard,
        Some(value) => SecurityMode::from_config_value(value).ok_or_else(|| {
            ExecutionAuthorityError::InvalidDefaultPermissionMode {
                value: value.to_string(),
                accepted: SecurityMode::NAMES
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            }
        })?,
    };

    let mode = if cli.yolo {
        SecurityMode::Yolo
    } else if cli.accept_all {
        SecurityMode::Standard
    } else if cli.read_only {
        SecurityMode::ReadOnly
    } else if cli.guarded {
        SecurityMode::Guarded
    } else if cli.restrictive {
        SecurityMode::Restrictive
    } else if cfg.yolo.unwrap_or(false) {
        SecurityMode::Yolo
    } else if cfg.accept_all.unwrap_or(false) {
        SecurityMode::Standard
    } else if cfg.restrictive.unwrap_or(false) {
        SecurityMode::Restrictive
    } else {
        configured_default
    };

    let tools_enabled = !cli.resolve_no_tools(cfg);
    let permission_checks_enabled = tools_enabled && !cli.dangerously_skip_permissions;
    let sandbox = match sandbox_policy {
        crate::sandbox::SandboxPolicy::Disabled => SandboxResolution::Disabled,
        crate::sandbox::SandboxPolicy::RequiredAndAvailable => SandboxResolution::Enforced,
        crate::sandbox::SandboxPolicy::RequiredButUnavailable
            if cli.sandbox_explicitly_requested(cfg) =>
        {
            return Err(ExecutionAuthorityError::SandboxUnavailable {
                backend: sandbox_backend.to_string(),
            });
        }
        crate::sandbox::SandboxPolicy::RequiredButUnavailable => {
            SandboxResolution::DegradedUnavailable
        }
    };

    Ok(ResolvedExecutionAuthority {
        mode,
        tools_enabled,
        permission_checks_enabled,
        sandbox,
    })
}

/// Resolve authority against the configured sandbox and materialize the
/// sandbox selected by that decision. Both interactive startup and ACP call
/// this before constructing execution machinery.
pub(crate) fn resolve_configured_execution_authority(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
) -> Result<(ResolvedExecutionAuthority, crate::sandbox::Sandbox), ExecutionAuthorityError> {
    let backend = cli.resolve_sandbox_backend(cfg);
    let configured = crate::sandbox::Sandbox::new(cli.resolve_sandbox(cfg), &backend)
        .with_windows_appcontainer_roots(
            cli.resolve_windows_appcontainer_read_roots(cfg),
            cli.resolve_windows_appcontainer_write_roots(cfg),
        );
    let policy = if cli.general_sandbox_is_eligible(cfg) {
        configured.policy()
    } else {
        crate::sandbox::SandboxPolicy::Disabled
    };
    let authority = resolve_execution_authority(cli, cfg, policy, &backend)?;
    let sandbox = if authority.sandbox == SandboxResolution::DegradedUnavailable {
        tracing::warn!(
            "sandbox backend '{backend}' was not found — continuing UNSANDBOXED; pass --sandbox to fail closed instead"
        );
        crate::sandbox::Sandbox::new(false, &backend).with_unavailable_default_fallback()
    } else {
        configured
    };

    Ok((authority, sandbox))
}

/// Resolve the model-visible shell exactly once from the invocation's
/// captured workspace and PATH. Tool-free modes intentionally perform no
/// executable lookup.
pub(crate) fn bind_configured_shell(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
    authority: ResolvedExecutionAuthority,
    workspace: &crate::paths::WorkspaceBinding,
    search_path: Option<&std::ffi::OsStr>,
    sandbox: crate::sandbox::Sandbox,
) -> crate::sandbox::Sandbox {
    if !authority.tools_enabled || !cli.tool_is_eligible(cfg, "shell") {
        return sandbox.with_resolved_shell(None);
    }
    let configured = cli.resolve_shell(cfg);
    let capability =
        crate::sandbox::ShellCapability::resolve(&configured, workspace.root(), search_path);
    if capability.is_none() {
        tracing::warn!(shell = %configured, "configured shell is unavailable or unsupported; shell tool disabled");
    }
    sandbox.with_bound_resolved_shell(capability, workspace)
}

/// Build a permission policy and approval channel for interactive startup.
pub(crate) fn build_interactive_permission(
    cfg: &crate::config::Config,
    authority: ResolvedExecutionAuthority,
) -> anyhow::Result<(
    Option<checker::PermCheck>,
    Option<ask::AskSender>,
    Option<ask::AskReceiver>,
)> {
    build_interactive_permission_at(cfg, authority, None)
}

pub(crate) fn build_interactive_permission_at(
    cfg: &crate::config::Config,
    authority: ResolvedExecutionAuthority,
    working_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<(
    Option<checker::PermCheck>,
    Option<ask::AskSender>,
    Option<ask::AskReceiver>,
)> {
    let Some(permission) = build_permission_checker_at(cfg, authority, working_dir)? else {
        return Ok((None, None, None));
    };
    let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(64);
    Ok((Some(permission), Some(ask_tx), Some(ask_rx)))
}

/// Build a permission policy for a frontend that cannot securely prompt.
///
/// The checker still preserves explicit allow and deny rules, but `Ask` has
/// no response channel and therefore fails closed in the shared tool gates.
pub(crate) fn build_noninteractive_permission(
    cfg: &crate::config::Config,
    authority: ResolvedExecutionAuthority,
) -> anyhow::Result<(Option<checker::PermCheck>, Option<ask::AskSender>)> {
    build_noninteractive_permission_at(cfg, authority, None)
}

pub(crate) fn build_noninteractive_permission_at(
    cfg: &crate::config::Config,
    authority: ResolvedExecutionAuthority,
    working_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<(Option<checker::PermCheck>, Option<ask::AskSender>)> {
    Ok((
        build_permission_checker_at(cfg, authority, working_dir)?,
        None,
    ))
}

fn build_permission_checker_at(
    cfg: &crate::config::Config,
    authority: ResolvedExecutionAuthority,
    working_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<Option<checker::PermCheck>> {
    // Parse and compile the complete policy before honoring flags that disable
    // tools or checks. Invalid configuration must fail closed in every mode.
    let configs = cfg.build_permission_config()?;
    let checker = checker::PermissionChecker::new(
        &configs,
        authority.mode,
        working_dir,
        cfg.permission_modes.clone(),
    )?;

    if !authority.tools_enabled || !authority.permission_checks_enabled {
        return Ok(None);
    }

    let permission = std::sync::Arc::new(std::sync::Mutex::new(checker));
    Ok(Some(permission))
}

pub(crate) async fn verify_acp_permission_policy() -> anyhow::Result<()> {
    let cli = crate::cli::Cli {
        guarded: true,
        ..Default::default()
    };
    let cfg = crate::config::Config {
        permission: Some(serde_json::json!({"write": "ask"})),
        permission_modes: Some(vec!["guarded".to_string()]),
        ..Default::default()
    };
    let (authority, _) = resolve_configured_execution_authority(&cli, &cfg)?;
    let (permission, ask_tx) = build_noninteractive_permission(&cfg, authority)?;

    anyhow::ensure!(
        ask_tx.is_none(),
        "ACP non-interactive policy exposed an approval channel"
    );
    let error =
        crate::agent::tools::check_perm(&permission, &ask_tx, "write", "policy-check").await;
    anyhow::ensure!(
        matches!(
            error,
            Err(crate::agent::tools::ToolError::Msg(ref message))
                if message == "Permission denied (non-interactive mode)"
        ),
        "ACP non-interactive Ask did not fail closed"
    );

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SecurityMode {
    Standard,
    Restrictive,
    ReadOnly,
    PlanWrite,
    Guarded,
    Yolo,
}

impl SecurityMode {
    /// Every accepted mode name, in the order documented in CONFIG.md.
    pub const NAMES: [&'static str; 6] = [
        "standard",
        "restrictive",
        "readonly",
        "planwrite",
        "guarded",
        "yolo",
    ];

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "standard" => Some(SecurityMode::Standard),
            "restrictive" => Some(SecurityMode::Restrictive),
            "readonly" => Some(SecurityMode::ReadOnly),
            "planwrite" => Some(SecurityMode::PlanWrite),
            "guarded" => Some(SecurityMode::Guarded),
            "yolo" => Some(SecurityMode::Yolo),
            _ => None,
        }
    }

    /// Parse a `default_permission_mode` config value. `accept` is a legacy
    /// alias for `standard`.
    pub fn from_config_value(s: &str) -> Option<Self> {
        match s {
            "accept" => Some(SecurityMode::Standard),
            other => Self::from_str(other),
        }
    }

    /// Relative authority granted by a mode. A prompt `%%mode=` directive may
    /// only move to a mode whose rank is at most the user's selected mode, so
    /// prompt content can narrow but never widen what the model may do.
    pub(crate) fn privilege_rank(self) -> u8 {
        match self {
            SecurityMode::ReadOnly => 0,
            SecurityMode::PlanWrite => 1,
            SecurityMode::Restrictive => 2,
            SecurityMode::Guarded => 3,
            SecurityMode::Standard => 4,
            SecurityMode::Yolo => 5,
        }
    }
}

impl std::fmt::Display for SecurityMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecurityMode::Standard => write!(f, "standard"),
            SecurityMode::Restrictive => write!(f, "restrictive"),
            SecurityMode::ReadOnly => write!(f, "readonly"),
            SecurityMode::PlanWrite => write!(f, "planwrite"),
            SecurityMode::Guarded => write!(f, "guarded"),
            SecurityMode::Yolo => write!(f, "yolo"),
        }
    }
}

/// Parse a `%%mode=X` directive from the first line of a prompt file.
/// Returns the mode string (e.g. "restrictive", "last_user_mode") if found.
/// Also returns the content with the directive line stripped.
pub fn parse_prompt_mode(content: &str) -> (Option<&str>, &str) {
    let Some(first) = content.lines().next() else {
        return (None, content);
    };
    let trimmed = first.trim();
    if let Some(mode_str) = trimmed.strip_prefix("%%mode=") {
        let mode_str = mode_str.trim();
        if mode_str.is_empty() {
            return (None, content);
        }
        // Strip the first line from the content
        let rest = if let Some(pos) = content.find('\n') {
            &content[pos + 1..]
        } else {
            ""
        };
        (Some(mode_str), rest)
    } else {
        (None, content)
    }
}

/// Resolve the security mode requested by prompt `name`'s `%%mode=`
/// directive, reading the raw (unstripped) prompt content from `prompts`.
/// Returns `None` when the prompt is unknown, has no directive, names an
/// unknown mode, or uses `last_user_mode` (meaningless at startup: the
/// current mode already is the user's mode).
pub fn resolve_startup_prompt_mode(
    prompts: &HashMap<String, String>,
    name: &str,
) -> Option<SecurityMode> {
    let content = prompts.get(name)?;
    let (mode_directive, _) = parse_prompt_mode(content);
    let mode_str = mode_directive?;
    if mode_str == "last_user_mode" {
        return None;
    }
    SecurityMode::from_str(mode_str)
}

/// Auto-deny regex patterns that are always active regardless of config.
/// These are appended to the end of each relevant tool's rules, so they
/// take precedence over user-configured allow/ask entries.
pub fn default_deny_regex_rules() -> Vec<(/* tool */ &'static str, /* regex */ &'static str)> {
    vec![("bash", r"^rm\s+.*\*")]
}

pub fn default_bash_rules() -> Vec<(&'static str, Action)> {
    vec![
        ("pwd", Action::Allow),
        ("git status", Action::Allow),
        ("cargo check", Action::Allow),
        ("cargo build", Action::Allow),
        ("cargo test", Action::Allow),
        ("cargo fmt", Action::Allow),
        ("cargo clippy", Action::Allow),
        ("pip list", Action::Allow),
        ("rm -rf /**", Action::Deny),
        ("sudo rm -rf /**", Action::Deny),
        ("dd **", Action::Deny),
        ("mkfs **", Action::Deny),
        ("fdisk **", Action::Deny),
        ("mkswap **", Action::Deny),
        ("editor **", Action::Deny),
        ("vim **", Action::Deny),
        ("vi **", Action::Deny),
        ("nano **", Action::Deny),
    ]
}

#[cfg(test)]
mod execution_authority_tests {
    use super::{
        SandboxResolution, SecurityMode, bind_configured_shell, build_interactive_permission,
        build_noninteractive_permission, resolve_configured_execution_authority,
        resolve_execution_authority,
    };
    use crate::cli::Cli;
    use crate::config::Config;
    use crate::sandbox::SandboxPolicy;

    struct Case {
        name: &'static str,
        cli: Cli,
        cfg: Config,
        sandbox_policy: SandboxPolicy,
        expected_mode: SecurityMode,
        tools_enabled: bool,
        permission_checks_enabled: bool,
        sandbox: Result<SandboxResolution, &'static str>,
    }

    #[test]
    fn execution_authority_precedence_matrix_is_frontend_independent() {
        let cases = vec![
            Case {
                name: "default",
                cli: Cli::default(),
                cfg: Config::default(),
                sandbox_policy: SandboxPolicy::RequiredAndAvailable,
                expected_mode: SecurityMode::Standard,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Enforced),
            },
            Case {
                name: "cli yolo outranks every lower permission mode",
                cli: Cli {
                    yolo: true,
                    accept_all: true,
                    read_only: true,
                    guarded: true,
                    restrictive: true,
                    ..Cli::default()
                },
                cfg: Config {
                    default_permission_mode: Some("readonly".to_string()),
                    ..Config::default()
                },
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::Yolo,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "cli accept all overrides config yolo",
                cli: Cli {
                    accept_all: true,
                    ..Cli::default()
                },
                cfg: Config {
                    yolo: Some(true),
                    ..Config::default()
                },
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::Standard,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "accept all outranks read only",
                cli: Cli {
                    accept_all: true,
                    read_only: true,
                    ..Cli::default()
                },
                cfg: Config::default(),
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::Standard,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "cli guarded overrides config accept all",
                cli: Cli {
                    guarded: true,
                    ..Cli::default()
                },
                cfg: Config {
                    accept_all: Some(true),
                    ..Config::default()
                },
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::Guarded,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "cli read only overrides permissive config booleans",
                cli: Cli {
                    read_only: true,
                    ..Cli::default()
                },
                cfg: Config {
                    yolo: Some(true),
                    accept_all: Some(true),
                    ..Config::default()
                },
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::ReadOnly,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "read only outranks guarded and restrictive",
                cli: Cli {
                    read_only: true,
                    guarded: true,
                    restrictive: true,
                    ..Cli::default()
                },
                cfg: Config::default(),
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::ReadOnly,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "guarded outranks restrictive",
                cli: Cli {
                    guarded: true,
                    restrictive: true,
                    ..Cli::default()
                },
                cfg: Config::default(),
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::Guarded,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "config restrictive outranks default mode",
                cli: Cli::default(),
                cfg: Config {
                    restrictive: Some(true),
                    default_permission_mode: Some("guarded".to_string()),
                    ..Config::default()
                },
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::Restrictive,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "configured default readonly",
                cli: Cli::default(),
                cfg: Config {
                    default_permission_mode: Some("readonly".to_string()),
                    ..Config::default()
                },
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::ReadOnly,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "no tools disables tools and permission checks",
                cli: Cli {
                    no_tools: true,
                    guarded: true,
                    ..Cli::default()
                },
                cfg: Config::default(),
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::Guarded,
                tools_enabled: false,
                permission_checks_enabled: false,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "config no tools disables tools and permission checks",
                cli: Cli {
                    guarded: true,
                    ..Cli::default()
                },
                cfg: Config {
                    no_tools: Some(true),
                    ..Config::default()
                },
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::Guarded,
                tools_enabled: false,
                permission_checks_enabled: false,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "dangerous bypass disables checks but retains tools",
                cli: Cli {
                    dangerously_skip_permissions: true,
                    guarded: true,
                    ..Cli::default()
                },
                cfg: Config::default(),
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::Guarded,
                tools_enabled: true,
                permission_checks_enabled: false,
                sandbox: Ok(SandboxResolution::Disabled),
            },
            Case {
                name: "default unavailable sandbox degrades explicitly",
                cli: Cli::default(),
                cfg: Config::default(),
                sandbox_policy: SandboxPolicy::RequiredButUnavailable,
                expected_mode: SecurityMode::Standard,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::DegradedUnavailable),
            },
            Case {
                name: "cli explicit unavailable sandbox rejects",
                cli: Cli {
                    sandbox: true,
                    ..Cli::default()
                },
                cfg: Config::default(),
                sandbox_policy: SandboxPolicy::RequiredButUnavailable,
                expected_mode: SecurityMode::Standard,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Err(
                    "sandbox backend 'missing' was not found — refusing to start with unsandboxed execution (use --no-sandbox to disable sandboxing explicitly)",
                ),
            },
            Case {
                name: "config explicit unavailable sandbox rejects",
                cli: Cli::default(),
                cfg: Config {
                    sandbox: Some(true),
                    ..Config::default()
                },
                sandbox_policy: SandboxPolicy::RequiredButUnavailable,
                expected_mode: SecurityMode::Standard,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Err(
                    "sandbox backend 'missing' was not found — refusing to start with unsandboxed execution (use --no-sandbox to disable sandboxing explicitly)",
                ),
            },
            Case {
                name: "no sandbox outranks explicit config",
                cli: Cli {
                    no_sandbox: true,
                    ..Cli::default()
                },
                cfg: Config {
                    sandbox: Some(true),
                    ..Config::default()
                },
                sandbox_policy: SandboxPolicy::Disabled,
                expected_mode: SecurityMode::Standard,
                tools_enabled: true,
                permission_checks_enabled: true,
                sandbox: Ok(SandboxResolution::Disabled),
            },
        ];

        for case in cases {
            let result =
                resolve_execution_authority(&case.cli, &case.cfg, case.sandbox_policy, "missing");
            match case.sandbox {
                Ok(sandbox) => {
                    let authority = result.unwrap_or_else(|error| {
                        panic!("{} unexpectedly rejected: {error}", case.name)
                    });
                    assert_eq!(authority.mode, case.expected_mode, "{}", case.name);
                    assert_eq!(authority.tools_enabled, case.tools_enabled, "{}", case.name);
                    assert_eq!(
                        authority.permission_checks_enabled, case.permission_checks_enabled,
                        "{}",
                        case.name
                    );
                    assert_eq!(authority.sandbox, sandbox, "{}", case.name);

                    let (interactive, interactive_ask, interactive_receiver) =
                        build_interactive_permission(&case.cfg, authority).unwrap();
                    assert_eq!(
                        interactive.is_some(),
                        case.permission_checks_enabled,
                        "{} interactive checker",
                        case.name
                    );
                    assert_eq!(
                        interactive_ask.is_some(),
                        case.permission_checks_enabled,
                        "{} interactive approval sender",
                        case.name
                    );
                    assert_eq!(
                        interactive_receiver.is_some(),
                        case.permission_checks_enabled,
                        "{} interactive approval receiver",
                        case.name
                    );

                    let (noninteractive, noninteractive_ask) =
                        build_noninteractive_permission(&case.cfg, authority).unwrap();
                    assert_eq!(
                        noninteractive.is_some(),
                        case.permission_checks_enabled,
                        "{} ACP checker",
                        case.name
                    );
                    assert!(noninteractive_ask.is_none(), "{} ACP Ask", case.name);
                }
                Err(message) => assert_eq!(
                    result.expect_err(case.name).to_string(),
                    message,
                    "{}",
                    case.name
                ),
            }
        }
    }

    #[test]
    fn configured_authority_rejects_an_explicitly_unavailable_sandbox() {
        let cli = Cli {
            sandbox: true,
            sandbox_backend: Some("definitely-not-a-real-backend".to_string()),
            ..Cli::default()
        };

        let error = resolve_configured_execution_authority(&cli, &Config::default())
            .expect_err("an explicit unavailable sandbox must fail closed");

        assert_eq!(
            error.to_string(),
            "sandbox backend 'definitely-not-a-real-backend' was not found — refusing to start with unsandboxed execution (use --no-sandbox to disable sandboxing explicitly)"
        );
    }

    #[test]
    fn configured_authority_rejects_configured_unavailable_backend() {
        let cfg = Config {
            sandbox_backend: Some("definitely-not-a-real-backend".to_string()),
            ..Config::default()
        };

        let error = resolve_configured_execution_authority(&Cli::default(), &cfg)
            .expect_err("a configured backend is an explicit fail-closed selection");

        assert_eq!(
            error.to_string(),
            "sandbox backend 'definitely-not-a-real-backend' was not found — refusing to start with unsandboxed execution (use --no-sandbox to disable sandboxing explicitly)"
        );
    }

    #[test]
    fn non_process_tool_modes_do_not_probe_an_unavailable_sandbox() {
        let cfg = Config::default();
        for cli in [
            Cli {
                no_tools: true,
                sandbox_backend: Some("definitely-not-a-real-backend".to_string()),
                ..Cli::default()
            },
            Cli {
                tools: vec!["read".to_string()],
                sandbox_backend: Some("definitely-not-a-real-backend".to_string()),
                ..Cli::default()
            },
        ] {
            let (authority, _) = resolve_configured_execution_authority(&cli, &cfg).unwrap();
            assert_eq!(authority.sandbox, SandboxResolution::Disabled);
        }
    }

    #[test]
    fn no_tools_binds_no_shell_even_when_a_supported_executable_exists() {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-no-tools-shell-{}",
            uuid::Uuid::new_v4()
        ));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let executable = bin.join(if cfg!(windows) { "bash.exe" } else { "bash" });
        std::fs::write(&executable, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let workspace = crate::paths::WorkspaceBinding::capture(&root).unwrap();
        let cli = Cli {
            no_tools: true,
            shell: Some("bash".to_string()),
            no_sandbox: true,
            ..Cli::default()
        };
        let cfg = Config::default();
        let (authority, sandbox) = resolve_configured_execution_authority(&cli, &cfg).unwrap();
        let sandbox = bind_configured_shell(
            &cli,
            &cfg,
            authority,
            &workspace,
            Some(bin.as_os_str()),
            sandbox,
        );

        assert!(!authority.tools_enabled);
        assert!(sandbox.shell_capability().is_none());
        assert_eq!(
            sandbox.wrap_command("echo must-not-run").unwrap_err(),
            "configured shell is unavailable or unsupported"
        );

        drop((sandbox, workspace));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod acp_permission_policy_tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use rig::tool::Tool;

    use super::{SandboxResolution, build_noninteractive_permission, resolve_execution_authority};
    use crate::agent::tools::{ToolError, WriteArgs, WriteTool, check_perm};
    use crate::cli::Cli;
    use crate::config::Config;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mini_agent_acp_permission_policy_{}_{}",
                std::process::id(),
                sequence
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn policy(
        action: &str,
    ) -> (
        Option<crate::permission::checker::PermCheck>,
        Option<crate::permission::ask::AskSender>,
    ) {
        let cli = Cli {
            guarded: true,
            ..Default::default()
        };
        let cfg = Config {
            permission: Some(serde_json::json!({"write": action})),
            permission_modes: Some(vec!["guarded".to_string()]),
            ..Default::default()
        };
        let authority = resolve_execution_authority(
            &cli,
            &cfg,
            crate::sandbox::SandboxPolicy::Disabled,
            "unused",
        )
        .unwrap();
        assert_eq!(authority.sandbox, SandboxResolution::Disabled);
        build_noninteractive_permission(&cfg, authority).unwrap()
    }

    #[test]
    fn acp_policy_construction_rejects_invalid_regex_before_tools_exist() {
        let cfg = Config {
            permission_regex: Some(serde_json::json!({
                "write": {"[unterminated": "allow"}
            })),
            ..Default::default()
        };

        for cli in [
            Cli::default(),
            Cli {
                no_tools: true,
                ..Cli::default()
            },
            Cli {
                dangerously_skip_permissions: true,
                ..Cli::default()
            },
        ] {
            let authority = resolve_execution_authority(
                &cli,
                &cfg,
                crate::sandbox::SandboxPolicy::Disabled,
                "unused",
            )
            .unwrap();
            let error = build_noninteractive_permission(&cfg, authority)
                .err()
                .expect("invalid regex must fail ACP policy construction")
                .to_string();
            assert!(error.contains("permission-regex"), "{error}");
            assert!(error.contains("write"), "{error}");
            assert!(error.contains("[unterminated"), "{error}");
        }
    }

    fn message(result: Result<Option<String>, ToolError>) -> Option<String> {
        match result {
            Err(ToolError::Msg(message)) => Some(message),
            _ => None,
        }
    }

    #[tokio::test]
    async fn acp_permission_policy_preserves_explicit_allow_and_deny() {
        let (allow_permission, allow_ask_tx) = policy("allow");
        assert!(allow_ask_tx.is_none());
        assert!(
            check_perm(&allow_permission, &allow_ask_tx, "write", "allowed-path")
                .await
                .is_ok()
        );

        let (deny_permission, deny_ask_tx) = policy("deny");
        assert!(deny_ask_tx.is_none());
        let denial =
            message(check_perm(&deny_permission, &deny_ask_tx, "write", "denied-path").await)
                .expect("explicit deny must reject the tool call");
        assert!(denial.starts_with("Permission denied:"));
    }

    #[tokio::test]
    async fn acp_permission_policy_ask_fails_closed_without_a_side_effect() {
        let temp = TempDir::new();
        let target = temp.path().join("must-not-exist.txt");
        let (permission, ask_tx) = policy("ask");
        assert!(ask_tx.is_none());
        let tool = WriteTool::new(permission, ask_tx, None);

        let error = tool
            .call(WriteArgs {
                path: target.to_string_lossy().into_owned(),
                content: "must not be written".to_string(),
            })
            .await
            .expect_err("unanswered ACP Ask must deny the write");

        assert_eq!(
            error.to_string(),
            "Permission denied (non-interactive mode)"
        );
        assert!(
            !target.exists(),
            "permission denial must happen before the tool side effect"
        );
    }

    #[tokio::test]
    async fn acp_permission_policy_concurrent_asks_are_denied_and_isolated() {
        let (first_permission, first_ask_tx) = policy("ask");
        let (second_permission, second_ask_tx) = policy("ask");

        let concurrent = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(
                check_perm(&first_permission, &first_ask_tx, "write", "session-one"),
                check_perm(&second_permission, &second_ask_tx, "write", "session-two")
            )
        })
        .await
        .expect("headless ACP permission checks must not wait for a responder");

        for result in [concurrent.0, concurrent.1] {
            assert_eq!(
                message(result).as_deref(),
                Some("Permission denied (non-interactive mode)")
            );
        }
    }
}
