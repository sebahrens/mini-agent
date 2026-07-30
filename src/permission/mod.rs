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
    pub bash: Option<ToolPerm>,
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

impl From<PermissionConfig> for PermissionConfigs {
    fn from(glob: PermissionConfig) -> Self {
        PermissionConfigs {
            glob,
            regex: PermissionConfig::default(),
        }
    }
}

/// Build a permission policy for a frontend that cannot securely prompt.
///
/// The checker still preserves explicit allow and deny rules, but `Ask` has
/// no response channel and therefore fails closed in the shared tool gates.
pub(crate) fn build_noninteractive_permission(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
    mode: SecurityMode,
) -> (Option<checker::PermCheck>, Option<ask::AskSender>) {
    if cli.resolve_no_tools(cfg) || cli.dangerously_skip_permissions {
        return (None, None);
    }

    let checker = checker::PermissionChecker::new(
        &cfg.build_permission_config(),
        mode,
        None,
        cfg.permission_modes.clone(),
    );
    let permission = std::sync::Arc::new(std::sync::Mutex::new(checker));

    (Some(permission), None)
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
    let (permission, ask_tx) = build_noninteractive_permission(&cli, &cfg, SecurityMode::Guarded);

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
        ("ls **", Action::Allow),
        ("cd **", Action::Allow),
        ("pwd", Action::Allow),
        ("echo **", Action::Allow),
        ("which **", Action::Allow),
        ("type **", Action::Allow),
        ("cat **", Action::Allow),
        ("head **", Action::Allow),
        ("tail **", Action::Allow),
        ("wc **", Action::Allow),
        ("sort **", Action::Allow),
        ("uniq **", Action::Allow),
        ("cut **", Action::Allow),
        ("diff **", Action::Allow),
        ("grep **", Action::Allow),
        ("rg **", Action::Allow),
        ("find **", Action::Allow),
        ("fd **", Action::Allow),
        ("fdfind **", Action::Allow),
        ("git status", Action::Allow),
        ("git log **", Action::Allow),
        ("git diff **", Action::Allow),
        ("git show **", Action::Allow),
        ("git branch **", Action::Allow),
        ("cargo check", Action::Allow),
        ("cargo build", Action::Allow),
        ("cargo test", Action::Allow),
        ("cargo fmt", Action::Allow),
        ("cargo clippy", Action::Allow),
        ("cargo install **", Action::Allow),
        ("mkdir **", Action::Allow),
        ("touch **", Action::Allow),
        ("cp **", Action::Allow),
        ("npm run **", Action::Allow),
        ("pip list", Action::Allow),
        ("pip show **", Action::Allow),
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
mod acp_permission_policy_tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use rig::tool::Tool;

    use super::{SecurityMode, build_noninteractive_permission};
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
        build_noninteractive_permission(&cli, &cfg, SecurityMode::Guarded)
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
