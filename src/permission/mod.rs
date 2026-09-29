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
        report_degraded_sandbox_once(&backend, configured.unavailable_diagnostic());
        crate::sandbox::Sandbox::new(false, &backend).with_unavailable_default_fallback()
    } else {
        configured
    };

    Ok((authority, sandbox))
}

/// Resolve the shell exactly once from the invocation's captured workspace
/// and PATH. Configured validators also need it; model tool eligibility stays
/// independent. Tool-free modes without validation perform no executable lookup.
pub(crate) fn bind_configured_shell(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
    authority: ResolvedExecutionAuthority,
    workspace: &crate::paths::WorkspaceBinding,
    search_path: Option<&std::ffi::OsStr>,
    sandbox: crate::sandbox::Sandbox,
) -> crate::sandbox::Sandbox {
    let model_shell_enabled = authority.tools_enabled && cli.tool_is_eligible(cfg, "shell");
    if !model_shell_enabled && !cli.configured_validation_needs_shell(cfg) {
        return sandbox.with_resolved_shell(None);
    }
    let configured = cli.resolve_shell(cfg);
    let capability =
        crate::sandbox::ShellCapability::resolve(&configured, workspace.root(), search_path);
    if capability.is_none() {
        tracing::warn!(shell = %configured, "configured shell is unavailable or unsupported; shell tool disabled");
    }
    let sandbox = sandbox.with_bound_resolved_shell(capability, workspace);
    if model_shell_enabled {
        sandbox
    } else {
        sandbox.with_validation_only_shell()
    }
}

/// Build a permission policy and approval channel for interactive startup.
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
    let checker = checker::PermissionChecker::new_for_sandbox(
        &configs,
        authority.mode,
        working_dir,
        cfg.permission_modes.clone(),
        authority.sandbox,
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

    /// Every mode, in [`Self::NAMES`] order.
    pub fn all() -> impl Iterator<Item = Self> {
        Self::NAMES.iter().filter_map(|name| Self::from_str(name))
    }

    /// One-line summary of what the mode allows, shown by `/mode`.
    pub fn description(self) -> &'static str {
        match self {
            SecurityMode::Standard => "allow within CWD, ask for external",
            SecurityMode::Restrictive => "ask for all operations",
            SecurityMode::ReadOnly => "allow reads, deny everything else",
            SecurityMode::PlanWrite => "readonly, except writing the workspace plan file",
            SecurityMode::Guarded => "allow reads, ask for everything else",
            SecurityMode::Yolo => "allow all, ask for destructive bash",
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

/// Parse a `%%mode=X` directive from a prompt header. Prompt-mode directives
/// may be composed with `%%agent=Y` in either order; all recognized header
/// lines are stripped from the returned content.
pub fn parse_prompt_mode(content: &str) -> (Option<&str>, &str) {
    let directives = crate::context::prompts::parse_directives(content);
    (directives.mode, directives.content)
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
/// Best-effort recognizer for destructive shell commands that `yolo` still
/// asks about when no configured rule matched the script. Each command of the
/// script (split on newlines, `;`, `&`, and `|`, with leading `sudo`/`env`/
/// `command`/`exec` wrappers removed) is checked. Shell syntax can always
/// reshape a command, so this is a workflow guard rail, not containment.
pub(crate) fn is_destructive_shell_script(script: &str) -> bool {
    static DESTRUCTIVE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"^(?:",
            // rm with a recursive or force flag, in any flag spelling.
            r"rm\s+(?:\S+\s+)*-(?:-recursive|-force|[A-Za-z]*[rRf][A-Za-z]*)\b",
            r"|(?:dd|shred|wipefs|mkswap|fdisk|sfdisk|parted)\b",
            r"|mkfs(?:\.\w+)?\b",
            r"|find\b.*\s-delete\b",
            r"|(?:chmod|chown)\s+(?:\S+\s+)*-(?:-recursive|[A-Za-z]*R[A-Za-z]*)\b",
            r"|git\s+(?:\S+\s+)*(?:push\s+(?:.*\s)?(?:--force\S*|-f\b|--delete\b|:\S)|reset\s+(?:.*\s)?--hard\b|clean\s+(?:.*\s)?-[A-Za-z]*f|branch\s+(?:.*\s)?-D\b)",
            r")"
        ))
        .expect("trusted destructive-command regex must compile")
    });
    script
        .split(['\n', ';', '&', '|'])
        .map(|command| {
            let mut command = command.trim().trim_start_matches(['(', '{', '!', ' ']);
            loop {
                let stripped = ["sudo ", "env ", "command ", "exec ", "nohup ", "time "]
                    .iter()
                    .find_map(|wrapper| command.strip_prefix(wrapper));
                match stripped {
                    Some(rest) => command = rest.trim_start(),
                    None => break,
                }
            }
            command
        })
        .any(|command| DESTRUCTIVE.is_match(command))
}

pub fn default_deny_regex_rules() -> Vec<(/* tool */ &'static str, /* regex */ &'static str)> {
    vec![("bash", r"^rm\s+.*\*")]
}

/// Operator-facing notice for a session whose implicitly selected default
/// sandbox backend was unavailable. Every frontend shows the same text: the
/// TUI as a first-turn chat notice (plus the persistent `sandbox:off` status
/// segment), headless runs on stderr regardless of `RUST_LOG`.
#[cfg(test)]
pub(crate) fn degraded_sandbox_notice(backend: &str) -> String {
    degraded_sandbox_notice_with(backend, None)
}

/// The degraded-sandbox notice naming the backend's closed preflight
/// diagnostic (for bubblewrap: missing binary, AppArmor `userns`
/// restriction, or disabled user namespaces) when one was recorded.
pub(crate) fn degraded_sandbox_notice_with(backend: &str, diagnostic: Option<&str>) -> String {
    let remedy = if backend == "bwrap" {
        "install bubblewrap (and allow unprivileged user namespaces), "
    } else {
        ""
    };
    // The diagnostics are closed single-line constants; keep the notice one
    // line even if that ever changes.
    let cause = diagnostic
        .map(|diagnostic| format!(" Cause: {}.", diagnostic.replace(['\n', '\r'], " ")))
        .unwrap_or_default();
    format!(
        "warning: sandbox backend '{backend}' is unavailable — shell commands run UNSANDBOXED with your full user privileges.{cause} \
         Built-in auto-allows for build/test commands (cargo, pip, git status) are withheld and ask first. \
         To fix: {remedy}pass --sandbox (or set `sandbox = true`) to refuse to start instead, or pass --no-sandbox to run unsandboxed deliberately."
    )
}

/// The degraded-sandbox notice for a materialized session sandbox, or `None`
/// when it is contained or was disabled deliberately (`--no-sandbox`).
pub(crate) fn degraded_sandbox_notice_for(sandbox: &crate::sandbox::Sandbox) -> Option<String> {
    match sandbox.explicit_shell_boundary() {
        crate::sandbox::ExplicitShellBoundary::UnavailableDefaultFallback { backend } => Some(
            degraded_sandbox_notice_with(&backend, sandbox.unavailable_diagnostic()),
        ),
        _ => None,
    }
}

/// Write the degraded-sandbox notice directly to `out`. This deliberately
/// bypasses `tracing`, so `RUST_LOG=off` or `--log-level off` cannot hide it.
pub(crate) fn write_degraded_sandbox_notice(
    out: &mut dyn std::io::Write,
    backend: &str,
    diagnostic: Option<&str>,
) -> std::io::Result<()> {
    writeln!(out, "{}", degraded_sandbox_notice_with(backend, diagnostic))?;
    out.flush()
}

/// Report an unavailable-default fallback once per process on stderr (and at
/// `info` to the log file). Headless print, loop and ACP runs have no other
/// surface; the TUI additionally shows a chat notice and a status segment.
fn report_degraded_sandbox_once(backend: &str, diagnostic: Option<&str>) {
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    tracing::info!(
        backend,
        diagnostic,
        "default sandbox unavailable; running unsandboxed"
    );
    if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let _ = write_degraded_sandbox_notice(&mut std::io::stderr().lock(), backend, diagnostic);
    }
}

/// Built-in `bash` allows that execute workspace-controlled code or
/// configuration: `cargo` runs `build.rs`, proc-macros, test binaries and
/// toolchain overrides (`rust-toolchain.toml`, `.cargo/config.toml`), `pip`
/// imports site `.pth` hooks, and `git status` honours `core.fsmonitor` from
/// a repository config the model can write. With the sandbox enforced they
/// run contained; after an unavailable-default fallback they would run with
/// full user privileges, so they are withheld and resolve like any other
/// unmatched script (`ask` in `standard` and `guarded`).
pub(crate) const UNSANDBOXED_EXEC_DEFAULT_ALLOWS: &[&str] = &[
    "git status",
    "cargo check",
    "cargo build",
    "cargo test",
    "cargo fmt",
    "cargo clippy",
    "pip list",
];

/// Built-in `bash` rules for a session with the given sandbox resolution.
/// Deny rules are never dropped; only exec-capable allows are withheld when
/// the default sandbox silently degraded to unsandboxed execution.
pub(crate) fn default_bash_rules_for(sandbox: SandboxResolution) -> Vec<(&'static str, Action)> {
    let mut rules = default_bash_rules();
    if sandbox == SandboxResolution::DegradedUnavailable {
        rules.retain(|(pattern, action)| {
            *action != Action::Allow || !UNSANDBOXED_EXEC_DEFAULT_ALLOWS.contains(pattern)
        });
    }
    rules
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
        ExecutionAuthorityError, SandboxResolution, SecurityMode, bind_configured_shell,
        build_interactive_permission_at, build_noninteractive_permission,
        resolve_configured_execution_authority, resolve_execution_authority,
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
                        build_interactive_permission_at(&case.cfg, authority, None).unwrap();
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
                        "{} noninteractive checker",
                        case.name
                    );
                    assert!(
                        noninteractive_ask.is_none(),
                        "{} noninteractive Ask",
                        case.name
                    );
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
    fn configured_validation_keeps_explicit_sandbox_requirements_without_model_shell() {
        for no_tools in [false, true] {
            for command in [None, Some("  "), Some("true")] {
                let cli = Cli {
                    no_tools,
                    tools: vec!["read".into()],
                    sandbox: true,
                    sandbox_backend: Some("__missing_validation_backend__".into()),
                    ..Cli::default()
                };
                let cfg = Config {
                    verify_command: command.map(Into::into),
                    ..Config::default()
                };
                assert!(!cli.tool_is_eligible(&cfg, "shell"));
                let result = resolve_configured_execution_authority(&cli, &cfg);
                if command == Some("true") {
                    assert!(matches!(
                        result,
                        Err(ExecutionAuthorityError::SandboxUnavailable { .. })
                    ));
                } else {
                    assert_eq!(result.unwrap().0.sandbox, SandboxResolution::Disabled);
                }
                #[cfg(feature = "loop")]
                {
                    let cli = Cli {
                        loop_run: Some("true".into()),
                        ..cli
                    };
                    assert!(matches!(
                        resolve_configured_execution_authority(&cli, &cfg),
                        Err(ExecutionAuthorityError::SandboxUnavailable { .. })
                    ));
                    assert!(!cli.tool_is_eligible(&cfg, "shell"));
                }
            }
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

    fn default_bash_decision(
        cli: Cli,
        policy: SandboxPolicy,
        script: &str,
    ) -> (SandboxResolution, super::checker::CheckResult) {
        let cfg = Config::default();
        let authority = resolve_execution_authority(&cli, &cfg, policy, "missing").unwrap();
        let (permission, _) = build_noninteractive_permission(&cfg, authority).unwrap();
        let permission = permission.expect("tools and checks are enabled");
        let decision = permission.lock().unwrap().check("bash", script);
        (authority.sandbox, decision)
    }

    /// cfib7: a silently degraded default sandbox must not keep auto-allowing
    /// scripts that execute workspace-controlled code with full privileges.
    #[test]
    fn exec_capable_default_allows_ask_only_when_the_default_sandbox_degraded() {
        use super::checker::CheckResult;

        for script in super::UNSANDBOXED_EXEC_DEFAULT_ALLOWS {
            let (resolution, enforced) =
                default_bash_decision(Cli::default(), SandboxPolicy::RequiredAndAvailable, script);
            assert_eq!(resolution, SandboxResolution::Enforced);
            assert_eq!(enforced, CheckResult::Allowed, "{script} under Enforced");

            let (resolution, degraded) = default_bash_decision(
                Cli::default(),
                SandboxPolicy::RequiredButUnavailable,
                script,
            );
            assert_eq!(resolution, SandboxResolution::DegradedUnavailable);
            assert_eq!(
                degraded,
                CheckResult::Ask,
                "{script} under DegradedUnavailable"
            );
        }

        // The headline case, spelled out: `cargo test` asks when degraded.
        assert_eq!(
            default_bash_decision(
                Cli::default(),
                SandboxPolicy::RequiredButUnavailable,
                "cargo test"
            )
            .1,
            CheckResult::Ask
        );
        // Guarded mode asks too; nothing silently falls back to allow.
        assert_eq!(
            default_bash_decision(
                Cli {
                    guarded: true,
                    ..Cli::default()
                },
                SandboxPolicy::RequiredButUnavailable,
                "cargo build --workspace"
            )
            .1,
            CheckResult::Ask
        );
        // An explicit `--no-sandbox` is the operator's deliberate choice and
        // keeps the historical built-in allows.
        let (resolution, disabled) =
            default_bash_decision(Cli::default(), SandboxPolicy::Disabled, "cargo test");
        assert_eq!(resolution, SandboxResolution::Disabled);
        assert_eq!(disabled, CheckResult::Allowed);
        // Harmless allows and every built-in deny survive degradation.
        assert_eq!(
            default_bash_decision(Cli::default(), SandboxPolicy::RequiredButUnavailable, "pwd").1,
            CheckResult::Allowed
        );
        assert!(matches!(
            default_bash_decision(
                Cli::default(),
                SandboxPolicy::RequiredButUnavailable,
                "mkfs /dev/sda"
            )
            .1,
            CheckResult::Denied(_)
        ));
    }

    /// Every built-in allow is either inert or classified as exec-capable, so
    /// a new default allow cannot silently bypass the degraded-sandbox rule.
    #[test]
    fn every_builtin_bash_allow_is_classified_for_unsandboxed_sessions() {
        const INERT: &[&str] = &["pwd"];
        for (pattern, action) in super::default_bash_rules() {
            if action == super::Action::Allow {
                assert!(
                    INERT.contains(&pattern)
                        || super::UNSANDBOXED_EXEC_DEFAULT_ALLOWS.contains(&pattern),
                    "unclassified built-in allow `{pattern}`"
                );
            }
        }
        let degraded = super::default_bash_rules_for(SandboxResolution::DegradedUnavailable);
        let deny_count = |rules: &[(&str, super::Action)]| {
            rules
                .iter()
                .filter(|(_, action)| *action == super::Action::Deny)
                .count()
        };
        assert_eq!(
            deny_count(&degraded),
            deny_count(&super::default_bash_rules())
        );
    }

    #[test]
    fn degraded_notice_is_actionable() {
        let notice = super::degraded_sandbox_notice("bwrap");
        assert!(notice.contains("UNSANDBOXED"), "{notice}");
        assert!(notice.contains("install bubblewrap"), "{notice}");
        assert!(notice.contains("--sandbox"), "{notice}");
        assert!(notice.contains("--no-sandbox"), "{notice}");
        assert!(!notice.contains('\n'), "{notice}");
        assert!(!super::degraded_sandbox_notice("seatbelt").contains("bubblewrap"));
        assert!(!notice.contains("Cause:"), "{notice}");
    }

    /// The notice names the closed preflight cause (mini-agent-jj4qw), so an
    /// Ubuntu AppArmor `userns` restriction is visible without reading logs.
    #[test]
    fn degraded_notice_names_the_preflight_diagnostic() {
        let diagnostic = crate::sandbox::BWRAP_APPARMOR_USERNS_DIAGNOSTIC;
        let notice = super::degraded_sandbox_notice_with("bwrap", Some(diagnostic));
        assert!(notice.contains("UNSANDBOXED"), "{notice}");
        assert!(notice.contains(diagnostic), "{notice}");
        assert!(notice.contains("AppArmor"), "{notice}");
        assert!(notice.contains("--no-sandbox"), "{notice}");
        assert!(!notice.contains('\n'), "{notice}");
        let multiline = super::degraded_sandbox_notice_with("bwrap", Some("first\nsecond"));
        assert!(!multiline.contains('\n'), "{multiline}");

        let mut stderr = Vec::new();
        super::write_degraded_sandbox_notice(&mut stderr, "bwrap", Some(diagnostic)).unwrap();
        assert_eq!(String::from_utf8(stderr).unwrap(), format!("{notice}\n"));
    }

    /// Headless runs report the fallback on stderr through a direct write,
    /// not through `tracing`, so no log filter can suppress it; and the
    /// notice is derived from the materialized session sandbox.
    #[test]
    fn headless_degraded_notice_is_written_directly_and_only_when_degraded() {
        use crate::sandbox::Sandbox;

        let degraded = Sandbox::new(false, "bwrap").with_unavailable_default_fallback();
        let notice = super::degraded_sandbox_notice_for(&degraded).expect("degraded");
        assert_eq!(
            notice,
            super::degraded_sandbox_notice_with("bwrap", degraded.unavailable_diagnostic())
        );
        assert_eq!(
            super::degraded_sandbox_notice_for(&Sandbox::new(false, "bwrap")),
            None
        );

        let mut stderr = Vec::new();
        super::write_degraded_sandbox_notice(
            &mut stderr,
            "bwrap",
            degraded.unavailable_diagnostic(),
        )
        .unwrap();
        assert_eq!(String::from_utf8(stderr).unwrap(), format!("{notice}\n"));
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
                overwrite: false,
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
