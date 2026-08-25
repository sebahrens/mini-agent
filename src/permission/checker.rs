use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use smallvec::SmallVec;

#[cfg(feature = "mcp")]
use crate::extras::mcp::config::TrustedMcpServer;
use crate::permission::pattern::Pattern;
use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

pub type PermCheck = Arc<Mutex<PermissionChecker>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    Allowed,
    AllowedWithCoaching(String),
    Ask,
    Denied(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanWritePathDecision {
    Authorized,
    NotPlanFile,
    OutsideWorkspace,
    Unresolvable,
}

#[derive(Clone, Debug)]
struct PlanWriteRoot {
    configured: PathBuf,
    canonical: PathBuf,
    identity: crate::fs::CheckedMetadata,
}

fn canonical_working_dir(working_dir: &Path) -> anyhow::Result<(PathBuf, PlanWriteRoot)> {
    let canonical = std::fs::canonicalize(working_dir).map_err(|error| {
        anyhow::anyhow!(
            "failed to bind permission workspace {}: {error}",
            working_dir.display()
        )
    })?;
    let identity = crate::fs::checked_path_metadata(&canonical).map_err(|error| {
        anyhow::anyhow!(
            "failed to validate permission workspace {}: {error}",
            canonical.display()
        )
    })?;
    anyhow::ensure!(identity.is_dir(), "permission workspace is not a directory");
    Ok((
        canonical.clone(),
        PlanWriteRoot {
            configured: working_dir.to_path_buf(),
            canonical,
            identity,
        },
    ))
}

#[derive(Clone, Debug)]
pub(crate) struct PlanWriteAuthorization {
    root: PlanWriteRoot,
}

impl PlanWriteAuthorization {
    pub(crate) fn revalidate(&self) -> std::io::Result<()> {
        let current_root = std::fs::canonicalize(&self.root.configured)?;
        if current_root != self.root.canonical {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "PlanWrite workspace changed after authorization",
            ));
        }
        let current_identity = crate::fs::checked_path_metadata(&current_root)?;
        if !current_identity.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "PlanWrite workspace is no longer a directory",
            ));
        }
        crate::fs::ensure_same_file(&current_root, &self.root.identity, &current_identity)
    }
}

impl CheckResult {
    pub fn allowed_with_coaching(tool: &str, _input: &str, count: usize) -> Self {
        CheckResult::AllowedWithCoaching(format!(
            "Coaching: You've called {tool} on the same input {count} times in a row. \
             This looks like a loop — try a different approach.",
        ))
    }
}

pub struct PermissionChecker {
    rules: HashMap<String, Vec<(Pattern, Action)>>,
    default_action: Action,
    ext_dir_rules: Vec<(Pattern, Action)>,
    doom_loop_action: Action,
    working_dir: String,
    plan_write_root: Option<PlanWriteRoot>,
    session_allowlist: Vec<(String, Pattern)>,
    last_call: Option<(String, String)>,
    consecutive_repeat_count: usize,
    mode: SecurityMode,
    user_mode: SecurityMode,
    permission_modes: Vec<SecurityMode>,
    allow_all_mcp_calls: bool,
    cached_resolved_cwd: Option<PathBuf>,
    /// One-shot: the next `check`/`check_path` call for this tool is forced
    /// to `Ask`, consumed immediately after. Set by a hook `ask` verdict.
    #[cfg(feature = "hooks")]
    pending_forced_ask: Option<String>,
    /// One-shot: the next `check`/`check_path` call for this tool suppresses
    /// the prompt (`Allowed`), consumed immediately after. Set by a hook
    /// `allow` verdict. Never bypasses a deny rule (checked first).
    #[cfg(feature = "hooks")]
    pending_one_shot_allow: Option<String>,
}

impl PermissionChecker {
    /// Rebind relative path authorization to an explicitly selected workspace.
    /// Worktree switching uses this instead of mutating process-global CWD.
    pub(crate) fn rebind_working_dir(&mut self, working_dir: &Path) -> anyhow::Result<()> {
        let (canonical, plan_write_root) = canonical_working_dir(working_dir)?;
        self.working_dir = canonical.to_string_lossy().into_owned();
        self.plan_write_root = Some(plan_write_root);
        self.cached_resolved_cwd = resolve_path_allow_missing(Path::new(&self.working_dir));
        Ok(())
    }

    fn compile_config(
        config: &PermissionConfig,
        config_field: &str,
        is_regex: bool,
    ) -> anyhow::Result<HashMap<String, Vec<(Pattern, Action)>>> {
        let mut rules: HashMap<String, Vec<(Pattern, Action)>> = HashMap::new();
        for (tool_name, tool_perm) in [
            ("shell", &config.bash),
            ("git/status", &config.git_status),
            ("git/diff", &config.git_diff),
            ("git/log", &config.git_log),
            ("git/show", &config.git_show),
            ("git/stage", &config.git_stage),
            ("git/unstage", &config.git_unstage),
            ("git/commit", &config.git_commit),
            ("js/fetch", &config.js_fetch),
            ("read", &config.read),
            ("write", &config.write),
            ("edit", &config.edit),
            ("grep", &config.grep),
            ("find_files", &config.find_files),
            ("list_dir", &config.list_dir),
            ("todo_write", &config.todo_write),
            ("mcp_tool", &config.mcp_tool),
        ] {
            let Some(tp) = tool_perm else { continue };
            let mut entries = Vec::new();
            match tp {
                ToolPerm::Simple(action) => {
                    let pat = if is_regex {
                        Pattern::new_regex(".*").expect("trusted match-all regex must compile")
                    } else {
                        // A simple permission applies to every input, including
                        // file paths whose components are separated by `/`.
                        Pattern::new("**")
                    };
                    entries.push((pat, *action));
                }
                ToolPerm::Granular(map) => {
                    for (pat, action) in map {
                        let pat = if is_regex {
                            Pattern::new_regex(pat).map_err(|error| {
                                anyhow::anyhow!(
                                    "invalid `{config_field}` rule for tool `{tool_name}` pattern `{pat}`: {error}"
                                )
                            })?
                        } else if is_path_tool_name(tool_name) {
                            Pattern::new_path(pat)
                        } else {
                            Pattern::new(pat)
                        };
                        entries.push((pat, *action));
                    }
                }
            }
            rules.insert(tool_name.to_string(), entries.clone());
            for alias in permission_tool_aliases(tool_name) {
                rules.insert((*alias).to_string(), entries.clone());
            }
        }
        Ok(rules)
    }

    pub fn new(
        configs: &PermissionConfigs,
        mode: SecurityMode,
        working_dir: Option<std::path::PathBuf>,
        permission_modes: Option<Vec<String>>,
    ) -> anyhow::Result<Self> {
        let default_action = configs
            .glob
            .default
            .or(configs.regex.default)
            .unwrap_or(Action::Allow);
        let doom_loop_action = configs
            .glob
            .doom_loop
            .or(configs.regex.doom_loop)
            .unwrap_or(Action::Ask);

        let mut rules = Self::compile_config(&configs.glob, "permission", false)?;
        let regex_rules = Self::compile_config(&configs.regex, "permission-regex", true)?;
        for (tool, entries) in regex_rules {
            let entry = rules.entry(tool).or_default();
            entry.extend(entries);
        }

        fn merge_entries(
            rules: &mut HashMap<String, Vec<(Pattern, Action)>>,
            entries: &Option<HashMap<String, Vec<String>>>,
            action: Action,
        ) {
            if let Some(map) = entries {
                for (tool, patterns) in map {
                    for alias in std::iter::once(tool.as_str())
                        .chain(permission_tool_aliases(tool).iter().copied())
                    {
                        let entry = rules.entry(alias.to_string()).or_default();
                        for pat in patterns {
                            let pattern = if is_path_tool_name(alias) {
                                Pattern::new_path(pat)
                            } else {
                                Pattern::new(pat)
                            };
                            entry.push((pattern, action));
                        }
                    }
                }
            }
        }

        merge_entries(&mut rules, &configs.glob.allow_entries, Action::Allow);
        merge_entries(&mut rules, &configs.glob.ask_entries, Action::Ask);
        merge_entries(&mut rules, &configs.glob.deny_entries, Action::Deny);

        if !rules.contains_key("shell") {
            let mut defaults = Vec::new();
            for (pat, action) in crate::permission::default_bash_rules() {
                defaults.push((Pattern::new(pat), action));
            }
            rules.insert("shell".to_string(), defaults.clone());
            rules.insert("bash".to_string(), defaults);
        }

        for (tool, regex) in crate::permission::default_deny_regex_rules() {
            let compiled =
                Pattern::new_regex(regex).expect("trusted built-in deny regex must compile");
            let canonical = canonical_permission_tool(tool);
            for key in
                std::iter::once(canonical).chain(permission_tool_aliases(canonical).iter().copied())
            {
                rules
                    .entry(key.to_string())
                    .or_default()
                    .push((compiled.clone(), Action::Deny));
            }
        }

        let mut ext_dir_rules: Vec<(Pattern, Action)> = configs
            .glob
            .external_directory
            .as_ref()
            .map(|map| {
                map.iter()
                    .map(|(pat, action)| (Pattern::new_path(&resolve_glob_pattern(pat)), *action))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(map) = &configs.regex.external_directory {
            for (pattern, action) in map {
                let compiled = Pattern::new_regex(pattern).map_err(|error| {
                    anyhow::anyhow!(
                        "invalid `permission-regex` rule for tool `external_directory` pattern `{pattern}`: {error}"
                    )
                })?;
                ext_dir_rules.push((compiled, *action));
            }
        }

        let working_dir = match working_dir {
            Some(working_dir) => working_dir,
            None => std::env::current_dir()?,
        };
        let (canonical_working_dir, plan_write_root) = canonical_working_dir(&working_dir)?;
        let working_dir = canonical_working_dir.to_string_lossy().into_owned();

        let resolved_modes: Vec<SecurityMode> = {
            let raw = permission_modes.unwrap_or_else(|| {
                vec![
                    "guarded".to_string(),
                    "standard".to_string(),
                    "yolo".to_string(),
                ]
            });
            raw.into_iter()
                .filter_map(|s| match s.as_str() {
                    "restrictive" => Some(SecurityMode::Restrictive),
                    "readonly" => Some(SecurityMode::ReadOnly),
                    "planwrite" => Some(SecurityMode::PlanWrite),
                    "guarded" => Some(SecurityMode::Guarded),
                    "standard" => Some(SecurityMode::Standard),
                    "yolo" => Some(SecurityMode::Yolo),
                    _ => None,
                })
                .collect()
        };

        let cached_resolved_cwd = resolve_path_allow_missing(Path::new(&working_dir));

        Ok(PermissionChecker {
            rules,
            default_action,
            ext_dir_rules,
            doom_loop_action,
            working_dir,
            plan_write_root: Some(plan_write_root),
            session_allowlist: Vec::new(),
            last_call: None,
            consecutive_repeat_count: 0,
            mode,
            user_mode: mode,
            permission_modes: resolved_modes,
            allow_all_mcp_calls: false,
            cached_resolved_cwd,
            #[cfg(feature = "hooks")]
            pending_forced_ask: None,
            #[cfg(feature = "hooks")]
            pending_one_shot_allow: None,
        })
    }

    /// Forces the next `check`/`check_path` call for `tool` to `Ask`,
    /// regardless of permission mode. Consumed after that one call. Set by a
    /// hook `ask` verdict; never overrides a deny rule (checked first).
    #[cfg(feature = "hooks")]
    pub fn force_ask_once(&mut self, tool: String) {
        self.pending_forced_ask = Some(canonical_permission_tool(&tool).to_string());
    }

    /// Suppresses the interactive prompt for the next `check`/`check_path`
    /// call for `tool`. Consumed after that one call. Set by a hook `allow`
    /// verdict; never overrides a deny rule (checked first).
    #[cfg(feature = "hooks")]
    pub fn allow_once(&mut self, tool: String) {
        self.pending_one_shot_allow = Some(canonical_permission_tool(&tool).to_string());
    }

    fn apply_rules(&self) -> bool {
        self.permission_modes.contains(&self.mode) || self.mode == SecurityMode::Yolo
    }

    fn is_read_tool(&self, tool: &str) -> bool {
        matches!(
            tool,
            "read"
                | "js/read_file"
                | "lsp_diagnostics"
                | "grep"
                | "find_files"
                | "list_dir"
                | "task"
                | "git/status"
                | "git/diff"
                | "git/log"
                | "git/show"
        )
    }

    fn resolve_check_action(&self, tool: &str, matched: &SmallVec<[Action; 4]>) -> Action {
        let base = matched.last().copied();
        match self.mode {
            SecurityMode::Restrictive => base.unwrap_or(Action::Ask),
            SecurityMode::ReadOnly | SecurityMode::PlanWrite => base.unwrap_or_else(|| {
                if self.is_read_tool(tool) {
                    Action::Allow
                } else {
                    Action::Deny
                }
            }),
            SecurityMode::Guarded => base.unwrap_or_else(|| {
                if self.is_read_tool(tool) {
                    Action::Allow
                } else {
                    Action::Ask
                }
            }),
            SecurityMode::Standard => base.unwrap_or({
                if matches!(tool, "shell" | "bash" | "js/fetch") {
                    // Bash scripts and network destinations are opaque,
                    // security-sensitive permission keys. An unmatched call
                    // must never inherit a permissive default.
                    Action::Ask
                } else {
                    self.default_action
                }
            }),
            SecurityMode::Yolo => match base {
                Some(Action::Deny) => Action::Deny,
                Some(other) => other,
                None => Action::Allow,
            },
        }
    }

    fn resolve_path_action(
        &self,
        tool: &str,
        matched: &SmallVec<[Action; 4]>,
        abs_path: &str,
        external: bool,
        external_action: Option<Action>,
        capability_contained: bool,
    ) -> Action {
        // External-directory deny is a second security baseline alongside
        // tool-specific deny rules. A broad read/LSP allow must never widen
        // access back into an explicitly denied external tree.
        if external && external_action == Some(Action::Deny) {
            return Action::Deny;
        }
        let base = matched.last().copied();
        match self.mode {
            SecurityMode::Restrictive => base.unwrap_or(Action::Ask),
            SecurityMode::ReadOnly => base.unwrap_or_else(|| {
                if self.is_read_tool(tool) {
                    Action::Allow
                } else {
                    Action::Deny
                }
            }),
            SecurityMode::PlanWrite => base.unwrap_or_else(|| {
                if self.is_read_tool(tool)
                    || (matches!(tool, "write" | "edit" | "js/write_file")
                        && self.plan_write_path_decision(abs_path)
                            == PlanWritePathDecision::Authorized)
                {
                    Action::Allow
                } else {
                    Action::Deny
                }
            }),
            SecurityMode::Guarded => base.unwrap_or_else(|| {
                if self.is_read_tool(tool) {
                    Action::Allow
                } else {
                    Action::Ask
                }
            }),
            SecurityMode::Standard => {
                let a = base.unwrap_or(self.default_action);
                let is_external = external && !capability_contained;
                if matched.is_empty() && self.is_path_tool(tool) && !is_external {
                    Action::Allow
                } else if matched.is_empty() && a == Action::Allow && is_external {
                    external_action.unwrap_or(Action::Ask)
                } else {
                    a
                }
            }
            SecurityMode::Yolo => match base {
                Some(Action::Deny) => Action::Deny,
                Some(other) => other,
                None => Action::Allow,
            },
        }
    }

    fn doom_loop_check(&mut self, tool: &str, doom_key: &str, action: Action) -> CheckResult {
        if action != Action::Deny {
            self.track_doom_loop(tool, doom_key);
            if self.is_doom_loop() {
                // doom_loop_action=Deny must block even allow-listed tools.
                if self.doom_loop_action == Action::Deny {
                    tracing::info!("perm doom-loop blocked: tool={}", tool);
                    return CheckResult::Denied(
                        "Doom loop: repeated identical tool call".to_string(),
                    );
                }
                if action == Action::Allow {
                    let count = self.count_doom_loop();
                    return CheckResult::allowed_with_coaching(tool, doom_key, count);
                }
                if self.doom_loop_action == Action::Ask {
                    tracing::info!("perm doom-loop ask: tool={}", tool);
                    return CheckResult::Ask;
                }
            }
        }
        match action {
            Action::Allow => CheckResult::Allowed,
            Action::Ask => CheckResult::Ask,
            Action::Deny => CheckResult::Denied("Blocked by permission rules".to_string()),
        }
    }

    /// Consumes a hook-set one-shot forced-ask/allow entry for `tool`, if
    /// pending. Called after the deny-rule check in `check`/`check_path` so
    /// neither can ever bypass a deny.
    #[cfg(feature = "hooks")]
    fn take_pending_one_shot(&mut self, tool: &str) -> Option<CheckResult> {
        if self.pending_forced_ask.as_deref() == Some(tool) {
            self.pending_forced_ask = None;
            return Some(CheckResult::Ask);
        }
        if self.pending_one_shot_allow.as_deref() == Some(tool) {
            self.pending_one_shot_allow = None;
            return Some(CheckResult::Allowed);
        }
        None
    }

    pub fn check(&mut self, tool: &str, input: &str) -> CheckResult {
        self.check_inner(tool, input, input, false)
    }

    /// Evaluate policy against a compatibility rendering while keeping a
    /// separate, reversible identity for session approval and doom-loop state.
    pub(crate) fn check_with_identity(
        &mut self,
        tool: &str,
        policy_input: &str,
        identity: &str,
    ) -> CheckResult {
        self.check_inner(tool, policy_input, identity, false)
    }

    #[cfg(feature = "mcp")]
    pub(crate) fn check_mcp(
        &mut self,
        input: &str,
        trusted_identity: Option<TrustedMcpServer>,
        mcp_tool_name: &str,
    ) -> CheckResult {
        let read_only_exempt =
            trusted_identity.is_some_and(|identity| identity.exempts_read_only_tool(mcp_tool_name));
        self.check_inner("mcp_tool", input, input, read_only_exempt)
    }

    fn check_inner(
        &mut self,
        tool: &str,
        policy_input: &str,
        identity: &str,
        mcp_read_only_exempt: bool,
    ) -> CheckResult {
        let tool = canonical_permission_tool(tool);
        tracing::debug!(
            "perm check: tool={}, input_len={}",
            tool,
            policy_input.len()
        );
        // Deny rules are the security baseline — evaluate before the session
        // allowlist and allow_all_mcp_calls so neither can bypass a deny.
        if self.matches_deny_rule(tool, &[policy_input, identity]) {
            return CheckResult::Denied("Blocked by deny rule".to_string());
        }
        if tool == "todo_write" {
            return CheckResult::Allowed;
        }
        #[cfg(feature = "hooks")]
        if let Some(result) = self.take_pending_one_shot(tool) {
            return result;
        }
        if self.allow_all_mcp_calls && tool == "mcp_tool" {
            return CheckResult::Allowed;
        }
        if self.is_session_allowed(tool, identity) {
            return CheckResult::Allowed;
        }
        if tool == "mcp_tool"
            && matches!(self.mode, SecurityMode::ReadOnly | SecurityMode::PlanWrite)
            && mcp_read_only_exempt
        {
            return CheckResult::Allowed;
        }

        let mut matched: SmallVec<[Action; 4]> = SmallVec::new();
        if self.apply_rules()
            && let Some(rules) = self.rules.get(tool)
        {
            for (pattern, action) in rules {
                let matches = if tool == "shell" && *action == Action::Allow {
                    // Model B: allow only the exact, complete script. Ask and
                    // deny rules remain pattern-based so broad safeguards keep
                    // working, but globs/regexes cannot widen Bash execution.
                    pattern.original == policy_input
                } else {
                    pattern.matches(policy_input)
                };
                if matches {
                    matched.push(*action);
                }
            }
        }

        let action = self.resolve_check_action(tool, &matched);
        self.doom_loop_check(tool, identity, action)
    }

    pub fn check_path(&mut self, tool: &str, path: &str) -> CheckResult {
        let tool = canonical_permission_tool(tool);
        tracing::debug!("perm check path: tool={}, path={}", tool, path);
        let expanded = crate::fs::expand_tilde(path);
        let abs_path = resolve_absolute(&expanded, &self.working_dir);

        let external = self.is_external_path(&abs_path);
        let external_action = if external {
            self.match_ext_dir(&abs_path)
        } else {
            None
        };
        self.check_pre_resolved_path(tool, &expanded, &abs_path, external, external_action)
    }

    /// Check a canonical permission key bound by the caller to an already-open
    /// filesystem object. No live path resolution occurs here: `external` was
    /// computed from canonical workspace and file paths when the handle was
    /// created, preventing a pathname swap from changing the policy subject.
    #[cfg(feature = "lsp")]
    pub(crate) fn check_canonical_path(
        &mut self,
        tool: &str,
        path: &str,
        external: bool,
    ) -> CheckResult {
        tracing::debug!("perm check bound path: tool={}, path={}", tool, path);
        let external_action = if external {
            self.match_ext_dir_bound(path)
        } else {
            None
        };
        self.check_pre_resolved_path(tool, path, path, external, external_action)
    }

    fn check_pre_resolved_path(
        &mut self,
        tool: &str,
        expanded: &str,
        abs_path: &str,
        external: bool,
        external_action: Option<Action>,
    ) -> CheckResult {
        // Deny rules first — security baseline, cannot be bypassed.
        if self.matches_deny_rule(tool, &[abs_path, expanded]) {
            return CheckResult::Denied("Blocked by deny rule".to_string());
        }
        if tool == "todo_write" {
            return CheckResult::Allowed;
        }
        // External-directory denies are the same kind of security baseline as
        // tool-specific denies. Evaluate them before hook one-shots and the
        // session AllowAlways list so a broad prior grant cannot reopen a
        // nested external tree that policy explicitly denies.
        if external && external_action == Some(Action::Deny) {
            return CheckResult::Denied("Blocked by external directory deny rule".to_string());
        }
        #[cfg(feature = "hooks")]
        if let Some(result) = self.take_pending_one_shot(tool) {
            return result;
        }
        if self.is_session_allowed(tool, expanded) || self.is_session_allowed(tool, abs_path) {
            return CheckResult::Allowed;
        }

        let mut matched: SmallVec<[Action; 4]> = SmallVec::new();
        if self.apply_rules()
            && let Some(rules) = self.rules.get(tool)
        {
            for (pattern, action) in rules {
                if pattern.matches_path(abs_path) || pattern.matches_path(expanded) {
                    matched.push(*action);
                }
            }
        }

        let action =
            self.resolve_path_action(tool, &matched, abs_path, external, external_action, false);
        self.doom_loop_check(tool, expanded, action)
    }

    /// Whether `path` is eligible for the narrow PlanWrite exception.
    ///
    /// Callers that mutate the filesystem use this to retain stable path
    /// identity across permission handling. Explicit permission rules still
    /// decide the final result in [`Self::check_path`].
    pub(crate) fn plan_write_authorization(
        &self,
        tool: &str,
        path: &str,
    ) -> Option<PlanWriteAuthorization> {
        (self.mode == SecurityMode::PlanWrite
            && matches!(tool, "write" | "edit" | "js/write_file")
            && self.plan_write_path_decision(path) == PlanWritePathDecision::Authorized)
            .then(|| PlanWriteAuthorization {
                root: self
                    .plan_write_root
                    .clone()
                    .expect("authorized root exists"),
            })
    }

    fn plan_write_path_decision(&self, path: &str) -> PlanWritePathDecision {
        if !is_plan_file(path) {
            return PlanWritePathDecision::NotPlanFile;
        }

        // The workspace is the authorization root. Its startup identity is
        // retained and revalidated at decision time so replacing the path
        // cannot redefine what PlanWrite is allowed to modify.
        let Some(root) = &self.plan_write_root else {
            return PlanWritePathDecision::Unresolvable;
        };
        if (PlanWriteAuthorization { root: root.clone() })
            .revalidate()
            .is_err()
        {
            return PlanWritePathDecision::Unresolvable;
        }
        let Some(target) = resolve_path_allow_missing(Path::new(path)) else {
            return PlanWritePathDecision::Unresolvable;
        };

        if target.starts_with(&root.canonical) {
            PlanWritePathDecision::Authorized
        } else {
            PlanWritePathDecision::OutsideWorkspace
        }
    }

    /// Check a capability-contained path without consulting its mutable
    /// ambient pathname. The caller must provide an absolute, lexically
    /// normalized identity beneath this checker's immutable workspace root.
    pub(crate) fn check_bound_path(&mut self, tool: &str, path: &str) -> CheckResult {
        tracing::debug!("perm check bound path: tool={}, path={}", tool, path);
        let path = Path::new(path);
        let working_dir = normalize_path(Path::new(&self.working_dir));
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
            || !normalize_path(path).starts_with(&working_dir)
        {
            return CheckResult::Denied("Invalid bound workspace path".to_string());
        }
        let normalized = normalize_path(path);
        let logical = normalized.to_string_lossy().into_owned();
        let relative = normalized
            .strip_prefix(&working_dir)
            .ok()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_string_lossy()
            .into_owned();

        if self.matches_deny_rule(tool, &[&logical, &relative]) {
            return CheckResult::Denied("Blocked by deny rule".to_string());
        }
        #[cfg(feature = "hooks")]
        if let Some(result) = self.take_pending_one_shot(tool) {
            return result;
        }
        if self.is_session_allowed(tool, &logical) || self.is_session_allowed(tool, &relative) {
            return CheckResult::Allowed;
        }

        let mut matched: SmallVec<[Action; 4]> = SmallVec::new();
        if self.apply_rules()
            && let Some(rules) = self.rules.get(tool)
        {
            for (pattern, action) in rules {
                if pattern.matches(&logical) || pattern.matches(&relative) {
                    matched.push(*action);
                }
            }
        }

        let action = self.resolve_path_action(tool, &matched, &logical, false, None, true);
        self.doom_loop_check(tool, &logical, action)
    }

    /// Check whether any deny rule matches the given inputs. Deny rules are
    /// the security baseline and must be evaluated before the session
    /// allowlist to prevent `AllowAlways` from bypassing them.
    fn matches_deny_rule(&self, tool: &str, inputs: &[&str]) -> bool {
        if !self.apply_rules() {
            return false;
        }
        if let Some(rules) = self.rules.get(tool) {
            for (pattern, action) in rules {
                let matches = |input: &&str| {
                    if is_path_tool_name(tool) {
                        pattern.matches_path(input)
                    } else {
                        pattern.matches(input)
                    }
                };
                if *action == Action::Deny && inputs.iter().any(matches) {
                    return true;
                }
            }
        }
        false
    }

    fn is_session_allowed(&self, tool: &str, input: &str) -> bool {
        for (allowed_tool, pattern) in &self.session_allowlist {
            let matches = if tool == "shell" {
                pattern.original == input
            } else if is_path_tool_name(tool) {
                pattern.matches_path(input)
            } else {
                pattern.matches(input)
            };
            if allowed_tool == tool && matches {
                return true;
            }
        }
        false
    }

    pub fn add_session_allowlist(&mut self, tool: String, pattern_str: &str) {
        let tool = canonical_permission_tool(&tool).to_string();
        let generated_path_scope = self
            .is_path_tool(&tool)
            .then(|| Pattern::new_generated_path_scope(pattern_str))
            .flatten();
        let has_generated = generated_path_scope.is_some();
        let pattern = if let Some(pattern) = generated_path_scope {
            pattern
        } else if self.is_path_tool(&tool) {
            Pattern::new_path(pattern_str)
        } else {
            Pattern::new(pattern_str)
        };
        self.session_allowlist.push((tool.clone(), pattern));
        if self.is_path_tool(&tool) && !has_generated {
            let expanded = crate::fs::expand_tilde(pattern_str);
            let abs = resolve_absolute(&expanded, &self.working_dir);
            if abs != expanded {
                self.session_allowlist.push((tool, Pattern::new_path(&abs)));
            }
        }
    }

    pub fn load_session_allowlist(&mut self, entries: &[(String, String)]) {
        for (tool, pat) in entries {
            let tool = canonical_permission_tool(tool).to_string();
            let generated_path_scope = self
                .is_path_tool(&tool)
                .then(|| Pattern::new_generated_path_scope(pat))
                .flatten();
            let has_generated = generated_path_scope.is_some();
            let pattern = if let Some(pattern) = generated_path_scope {
                pattern
            } else if self.is_path_tool(&tool) {
                Pattern::new_path(pat)
            } else {
                Pattern::new(pat)
            };
            self.session_allowlist.push((tool.clone(), pattern));
            if self.is_path_tool(&tool) && !has_generated {
                let expanded = crate::fs::expand_tilde(pat);
                let abs = resolve_absolute(&expanded, &self.working_dir);
                if abs != expanded {
                    self.session_allowlist
                        .push((tool.clone(), Pattern::new_path(&abs)));
                }
            }
        }
    }

    pub fn set_mode(&mut self, mode: SecurityMode) {
        tracing::debug!("perm mode changed: {:?} -> {:?}", self.mode, mode);
        self.mode = mode;
        self.user_mode = mode;
    }

    pub fn set_prompt_mode(&mut self, mode: SecurityMode) {
        self.mode = mode;
    }

    pub fn restore_user_mode(&mut self) {
        self.mode = self.user_mode;
    }

    pub fn mode(&self) -> SecurityMode {
        self.mode
    }

    #[cfg(feature = "mcp")]
    pub fn set_allow_all_mcp_calls(&mut self, allow: bool) {
        self.allow_all_mcp_calls = allow;
    }

    fn is_path_tool(&self, tool: &str) -> bool {
        is_path_tool_name(tool)
    }

    fn is_external_path(&mut self, path_str: &str) -> bool {
        let p = Path::new(path_str);
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(&self.working_dir).join(p)
        };
        let Some(normalized) = resolve_path_allow_missing(&p) else {
            return true;
        };
        let normalized_cwd = if let Some(cached) = &self.cached_resolved_cwd {
            if let Ok(metadata) = std::fs::symlink_metadata(cached) {
                if metadata.is_dir() {
                    Some(cached.clone())
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let normalized_cwd = normalized_cwd.or_else(|| {
            let resolved = resolve_path_allow_missing(Path::new(&self.working_dir));
            self.cached_resolved_cwd = resolved.clone();
            resolved
        });

        match normalized_cwd {
            Some(cwd) => !normalized.starts_with(&cwd),
            None => true, // Fail-closed: if we can't resolve cwd, treat as external
        }
    }

    fn match_ext_dir(&self, path_str: &str) -> Option<Action> {
        let resolved = resolve_path_allow_missing(Path::new(path_str))?;
        let resolved = resolved.to_string_lossy();
        self.match_ext_dir_bound(&resolved)
    }

    fn match_ext_dir_bound(&self, path_str: &str) -> Option<Action> {
        #[cfg(windows)]
        {
            // Callers may configure anchored raw regexes against either the
            // ordinary Win32 spelling or the canonical verbatim spelling.
            // Evaluate both without changing separators, and combine matches
            // fail-closed so a permissive spelling cannot mask a restrictive
            // one. This also covers bound LSP paths, which deliberately avoid
            // a second filesystem resolution after authorization.
            let ordinary = crate::permission::pattern::normalize_policy_path(path_str);
            let verbatim = windows_verbatim_policy_path(&ordinary);
            let mut decision = None;
            for (pattern, action) in &self.ext_dir_rules {
                if pattern.matches_path(path_str)
                    || pattern.matches_path(&ordinary)
                    || verbatim
                        .as_deref()
                        .is_some_and(|path| pattern.matches_path(path))
                {
                    decision = Some(match (decision, *action) {
                        (_, Action::Deny) | (Some(Action::Deny), _) => Action::Deny,
                        (_, Action::Ask) | (Some(Action::Ask), _) => Action::Ask,
                        _ => Action::Allow,
                    });
                }
            }
            return decision;
        }
        #[cfg(not(windows))]
        for (pattern, action) in &self.ext_dir_rules {
            if pattern.matches_path(path_str) {
                return Some(*action);
            }
        }
        #[cfg(not(windows))]
        None
    }

    /// Feeds a hook-denied call into doom-loop detection. A hook deny never
    /// reaches `check`/`check_path`, so without this a denied call could
    /// retry forever invisibly to doom detection.
    #[cfg(feature = "hooks")]
    pub fn record_blocked(&mut self, tool: &str, input: &str) {
        self.track_doom_loop(canonical_permission_tool(tool), input);
    }

    fn track_doom_loop(&mut self, tool: &str, input: &str) {
        let current = (tool.to_string(), input.to_string());
        match &self.last_call {
            Some(prev) if *prev == current => {
                self.consecutive_repeat_count += 1;
            }
            _ => {
                self.last_call = Some(current);
                self.consecutive_repeat_count = 1;
            }
        }
    }

    fn is_doom_loop(&self) -> bool {
        self.consecutive_repeat_count >= 3
    }

    fn count_doom_loop(&self) -> usize {
        self.consecutive_repeat_count
    }
}

fn is_path_tool_name(tool: &str) -> bool {
    matches!(
        tool,
        "read"
            | "write"
            | "edit"
            | "grep"
            | "find_files"
            | "list_dir"
            | "js/read_file"
            | "js/write_file"
            | "lsp_diagnostics"
    )
}

fn permission_tool_aliases(tool: &str) -> &'static [&'static str] {
    match tool {
        "shell" => &["bash"],
        "bash" => &["shell"],
        "read" => &["js/read_file", "lsp_diagnostics"],
        "write" => &["js/write_file"],
        _ => &[],
    }
}

fn canonical_permission_tool(tool: &str) -> &str {
    if tool == "bash" { "shell" } else { tool }
}

fn resolve_absolute(path: &str, working_dir: &str) -> String {
    let expanded = crate::fs::expand_tilde(path);
    let p = Path::new(&expanded);
    if p.is_absolute() {
        p.to_string_lossy().to_string()
    } else {
        Path::new(working_dir).join(p).to_string_lossy().to_string()
    }
}

#[cfg(windows)]
fn windows_verbatim_policy_path(path: &str) -> Option<String> {
    if path.starts_with(r"\\?\") {
        Some(path.to_string())
    } else if let Some(rest) = path.strip_prefix(r"\\") {
        Some(format!(r"\\?\UNC\{rest}"))
    } else {
        let bytes = path.as_bytes();
        (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
        .then(|| format!(r"\\?\{path}"))
    }
}

#[cfg(all(test, windows))]
mod windows_workspace_authority_tests {
    use super::*;

    #[test]
    fn windows_workspace_authority_drive_and_unc_spelling_parity() {
        assert_eq!(
            windows_verbatim_policy_path(r"C:\projects\mini-agent"),
            Some(r"\\?\C:\projects\mini-agent".to_string())
        );
        assert_eq!(
            windows_verbatim_policy_path(r"\\server\share\mini-agent"),
            Some(r"\\?\UNC\server\share\mini-agent".to_string())
        );

        let workspace = std::env::temp_dir().join(format!(
            "mini-agent-workspace-authority-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(&workspace).unwrap();
        let config = PermissionConfig {
            read: Some(ToolPerm::Simple(Action::Allow)),
            write: Some(ToolPerm::Simple(Action::Allow)),
            ..PermissionConfig::default()
        };
        let mut checker = PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(workspace.clone()),
            Some(vec!["standard".to_string()]),
        )
        .unwrap();

        assert_eq!(Path::new(&checker.working_dir), binding.root());
        let target = binding.root().join("gold-eiffel.js");
        assert_eq!(
            checker.check_bound_path("write", &target.to_string_lossy()),
            CheckResult::Allowed
        );
        checker.rebind_working_dir(binding.root()).unwrap();
        assert_eq!(
            checker.check_bound_path("read", &target.to_string_lossy()),
            CheckResult::Allowed
        );
        drop(binding);
        std::fs::remove_dir_all(workspace).unwrap();
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            other => {
                result.push(other);
            }
        }
    }
    result
}

/// Resolve symlinks in the existing portion of a path while permitting a
/// non-existent suffix. Errors other than a missing path fail closed.
fn resolve_path_allow_missing(path: &Path) -> Option<PathBuf> {
    fn resolve(path: &Path, symlink_depth: usize) -> Option<PathBuf> {
        if symlink_depth > 40 {
            return None;
        }

        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().ok()?.join(path)
        };
        let mut resolved = PathBuf::new();

        for component in absolute.components() {
            match component {
                std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                    resolved.push(component.as_os_str());
                }
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    resolved.pop();
                }
                std::path::Component::Normal(name) => {
                    resolved.push(name);
                    match std::fs::symlink_metadata(&resolved) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            let target = std::fs::read_link(&resolved).ok()?;
                            let target = if target.is_absolute() {
                                target
                            } else {
                                resolved.parent()?.join(target)
                            };
                            resolved = resolve(&target, symlink_depth + 1)?;
                        }
                        Ok(_) => {
                            resolved = resolved.canonicalize().ok()?;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => return None,
                    }
                }
            }
        }

        Some(resolved)
    }

    resolve(path, 0).map(|resolved| normalize_path(&resolved))
}

/// Canonicalize the non-pattern prefix so external-directory rules are matched
/// against the same resolved path used for workspace containment.
fn resolve_glob_pattern(pattern: &str) -> String {
    let expanded = crate::fs::expand_tilde(pattern);
    let Some(wildcard) = expanded.find(['*', '?']) else {
        return resolve_path_allow_missing(Path::new(&expanded))
            .unwrap_or_else(|| PathBuf::from(expanded))
            .to_string_lossy()
            .into_owned();
    };
    let Some(prefix_end) = expanded[..wildcard]
        .rfind(['/', '\\'])
        .map(|index| index + 1)
    else {
        return expanded;
    };
    let Some(prefix) = resolve_path_allow_missing(Path::new(&expanded[..prefix_end])) else {
        return expanded;
    };
    prefix
        .join(&expanded[prefix_end..])
        .to_string_lossy()
        .into_owned()
}

fn is_plan_file(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.starts_with("PLAN") && name.ends_with(".md"))
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "zerostack_permission_checker_test_{}_{}",
                std::process::id(),
                sequence
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn plan_write_checker(
        workspace: &Path,
        config: PermissionConfig,
        apply_rules: bool,
    ) -> PermissionChecker {
        PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::PlanWrite,
            Some(workspace.to_path_buf()),
            apply_rules.then(|| vec!["planwrite".to_string()]),
        )
        .expect("valid plan-write permission fixture")
    }

    #[test]
    fn plan_write_path_authorization_allows_workspace_plan_files() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        let plans = workspace.join("docs/plans");
        std::fs::create_dir_all(&plans).unwrap();
        let mut checker = plan_write_checker(&workspace, PermissionConfig::default(), false);

        for path in [
            workspace.join("PLAN.md"),
            plans.join("PLAN-security.md"),
            plans.join("PLAN-nonexistent.md"),
        ] {
            assert_eq!(
                checker.check_path("write", &path.to_string_lossy()),
                CheckResult::Allowed,
                "workspace-contained plan should be eligible: {}",
                path.display()
            );
        }
        assert!(matches!(
            checker.check_path("write", &plans.join("notes.md").to_string_lossy()),
            CheckResult::Denied(_)
        ));
    }

    #[test]
    fn plan_write_path_authorization_rejects_external_lookalikes() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        let sibling = temp.0.join("workspace-sibling");
        let external = temp.0.join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let mut checker = plan_write_checker(&workspace, PermissionConfig::default(), false);

        let home_lookalike = PathBuf::from(crate::fs::expand_tilde("~/PLAN-private.md"));
        let temp_lookalike = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("PLAN-outside-{}.md", std::process::id()));
        for path in [
            sibling.join("PLAN.md"),
            external.join("PLAN.md"),
            workspace.join("../external/PLAN-via-dotdot.md"),
            home_lookalike,
            temp_lookalike,
        ] {
            assert!(
                matches!(
                    checker.check_path("write", &path.to_string_lossy()),
                    CheckResult::Denied(_)
                ),
                "external plan lookalike must not receive PlanWrite privilege: {}",
                path.display()
            );
        }
    }

    #[test]
    fn plan_write_path_authorization_preserves_explicit_external_prompt_policy() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        let external = temp.0.join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let config = PermissionConfig {
            write: Some(ToolPerm::Simple(Action::Ask)),
            ..PermissionConfig::default()
        };
        let mut checker = plan_write_checker(&workspace, config, true);

        assert_eq!(
            checker.check_path("write", &external.join("PLAN.md").to_string_lossy()),
            CheckResult::Ask,
            "an ineligible lookalike must continue through ordinary configured policy"
        );
    }

    #[test]
    fn plan_write_path_authorization_never_overrides_explicit_deny() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = PermissionConfig {
            write: Some(ToolPerm::Simple(Action::Deny)),
            ..PermissionConfig::default()
        };
        let mut checker = plan_write_checker(&workspace, config, true);

        assert!(matches!(
            checker.check_path("write", &workspace.join("PLAN.md").to_string_lossy()),
            CheckResult::Denied(_)
        ));
    }

    #[test]
    fn plan_write_path_authorization_rejects_symlink_escapes() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        let external = temp.0.join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let external_plan = external.join("PLAN.md");
        std::fs::write(&external_plan, "sentinel").unwrap();
        symlink(&external, workspace.join("linked-parent")).unwrap();
        symlink(&external_plan, workspace.join("PLAN-linked.md")).unwrap();
        let mut checker = plan_write_checker(&workspace, PermissionConfig::default(), false);

        for path in [
            workspace.join("linked-parent/PLAN.md"),
            workspace.join("PLAN-linked.md"),
        ] {
            assert!(
                matches!(
                    checker.check_path("write", &path.to_string_lossy()),
                    CheckResult::Denied(_)
                ),
                "symlink escape must not receive PlanWrite privilege: {}",
                path.display()
            );
        }
        assert_eq!(std::fs::read_to_string(external_plan).unwrap(), "sentinel");
    }

    #[test]
    fn plan_write_path_authorization_rejects_workspace_root_replacement() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        let original_workspace = temp.0.join("original-workspace");
        let external = temp.0.join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let mut checker = plan_write_checker(&workspace, PermissionConfig::default(), false);

        std::fs::rename(&workspace, &original_workspace).unwrap();
        symlink(&external, &workspace).unwrap();

        for tool in ["write", "edit", "js/write_file"] {
            assert!(
                matches!(
                    checker.check_path(tool, &workspace.join("PLAN.md").to_string_lossy()),
                    CheckResult::Denied(_)
                ),
                "{tool} must reject a replaced workspace root"
            );
        }
        assert!(!external.join("PLAN.md").exists());
    }

    #[test]
    fn plan_write_path_authorization_accepts_originally_symlinked_workspace() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        let workspace_link = temp.0.join("workspace-link");
        std::fs::create_dir_all(&workspace).unwrap();
        symlink(&workspace, &workspace_link).unwrap();
        let mut checker = plan_write_checker(&workspace_link, PermissionConfig::default(), false);

        assert_eq!(
            checker.check_path("write", &workspace_link.join("PLAN.md").to_string_lossy()),
            CheckResult::Allowed
        );
    }

    #[test]
    fn external_directory_allow_does_not_follow_workspace_symlink() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        let external = temp.0.join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        symlink(&external, workspace.join("evil-link")).unwrap();

        let config = PermissionConfig {
            external_directory: Some(
                [(format!("{}/**", workspace.to_string_lossy()), Action::Allow)].into(),
            ),
            ..PermissionConfig::default()
        };
        let mut checker = PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(workspace.clone()),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission test configuration");

        let result = checker.check_path(
            "write",
            &workspace.join("evil-link/new-file").to_string_lossy(),
        );

        assert!(
            matches!(result, CheckResult::Ask),
            "resolved external target must not match the workspace allow rule, got {result:?}"
        );
    }

    #[test]
    fn bound_permission_identity_keeps_denied_root_during_pathname_swap() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        let replacement = temp.0.join("replacement");
        let retained = temp.0.join("workspace-retained");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&replacement).unwrap();
        std::fs::write(workspace.join("sentinel.txt"), "denied-original").unwrap();
        std::fs::write(replacement.join("sentinel.txt"), "allowed-replacement").unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let replacement = replacement.canonicalize().unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(&workspace).unwrap();
        let config = PermissionConfig {
            read: Some(ToolPerm::Granular(
                [
                    (format!("{}/**", workspace.to_string_lossy()), Action::Deny),
                    (
                        format!("{}/**", replacement.to_string_lossy()),
                        Action::Allow,
                    ),
                ]
                .into(),
            )),
            ..PermissionConfig::default()
        };
        let mut checker = PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(workspace.clone()),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid bound-workspace permission fixture");

        std::fs::rename(&workspace, &retained).unwrap();
        symlink(&replacement, &workspace).unwrap();
        let relative = Path::new("sentinel.txt");
        let logical = binding.logical_relative_path(relative).unwrap();
        let ambient = std::fs::canonicalize(workspace.join(relative)).unwrap();
        assert_eq!(
            checker.check_path("read", &ambient.to_string_lossy()),
            CheckResult::Allowed,
            "the mutable ambient replacement is intentionally configured as allowed"
        );
        assert_eq!(
            checker.check_bound_path("read", &logical.to_string_lossy()),
            CheckResult::Denied("Blocked by deny rule".to_string()),
            "the captured workspace's denied identity must not become the replacement identity"
        );

        std::fs::remove_file(&workspace).unwrap();
        std::fs::rename(&retained, &workspace).unwrap();
    }

    #[test]
    fn bound_permission_identity_matches_relative_path_rules() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        std::fs::create_dir_all(workspace.join("src/nested")).unwrap();
        let config = PermissionConfig {
            read: Some(ToolPerm::Granular(
                [
                    ("secret.txt".to_string(), Action::Deny),
                    ("src/**".to_string(), Action::Deny),
                    ("allowed/**".to_string(), Action::Allow),
                ]
                .into(),
            )),
            ..PermissionConfig::default()
        };
        let mut checker = PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Restrictive,
            Some(workspace.clone()),
            Some(vec!["restrictive".to_string()]),
        )
        .expect("valid relative-path permission fixture");

        assert_eq!(
            checker.check_bound_path("read", &workspace.join("secret.txt").to_string_lossy()),
            CheckResult::Denied("Blocked by deny rule".to_string())
        );
        assert_eq!(
            checker.check_bound_path(
                "read",
                &workspace.join("src/nested/value.rs").to_string_lossy()
            ),
            CheckResult::Denied("Blocked by deny rule".to_string())
        );
        assert_eq!(
            checker.check_bound_path(
                "read",
                &workspace.join("allowed/value.txt").to_string_lossy()
            ),
            CheckResult::Allowed,
            "a relative allow rule must match the capability-relative identity"
        );
        checker.add_session_allowlist("read".to_string(), "session/**");
        assert_eq!(
            checker.check_bound_path(
                "read",
                &workspace.join("session/value.txt").to_string_lossy()
            ),
            CheckResult::Allowed,
            "a relative session allowlist must match the capability-relative identity"
        );
    }

    #[test]
    fn js_file_tools_inherit_path_rules_and_component_containment() {
        let temp = TempDir::new();
        let workspace = temp.0.join("safe");
        let sibling = temp.0.join("safe-a");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let config = PermissionConfig {
            read: Some(ToolPerm::Simple(Action::Allow)),
            write: Some(ToolPerm::Simple(Action::Deny)),
            ..PermissionConfig::default()
        };
        let mut checker = PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(workspace.clone()),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission test configuration");

        assert_eq!(
            checker.check_path(
                "js/read_file",
                &workspace.join("inside.txt").to_string_lossy()
            ),
            CheckResult::Allowed
        );
        assert_eq!(
            checker.check_path(
                "js/write_file",
                &workspace.join("inside.txt").to_string_lossy()
            ),
            CheckResult::Denied("Blocked by deny rule".to_string())
        );

        let mut default_checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(workspace),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission test configuration");
        assert!(
            default_checker.is_external_path(&sibling.join("outside.txt").to_string_lossy()),
            "sibling-prefix path must remain outside the workspace"
        );
    }

    #[test]
    fn js_fetch_permissions_fail_closed_by_mode_and_honor_explicit_rules() {
        let new_checker = |mode, config| {
            PermissionChecker::new(
                &PermissionConfigs::from(config),
                mode,
                None,
                Some(vec![
                    "standard".to_string(),
                    "restrictive".to_string(),
                    "readonly".to_string(),
                    "planwrite".to_string(),
                    "guarded".to_string(),
                    "yolo".to_string(),
                ]),
            )
            .expect("valid permission test configuration")
        };
        for mode in [
            SecurityMode::Standard,
            SecurityMode::Restrictive,
            SecurityMode::Guarded,
        ] {
            assert_eq!(
                new_checker(mode, PermissionConfig::default()).check(
                    "js/fetch",
                    "https://example.com/ destinations=[93.184.216.34:443]"
                ),
                CheckResult::Ask
            );
        }
        for mode in [SecurityMode::ReadOnly, SecurityMode::PlanWrite] {
            assert!(matches!(
                new_checker(mode, PermissionConfig::default()).check(
                    "js/fetch",
                    "https://example.com/ destinations=[93.184.216.34:443]"
                ),
                CheckResult::Denied(_)
            ));
        }
        assert_eq!(
            new_checker(SecurityMode::Yolo, PermissionConfig::default()).check(
                "js/fetch",
                "https://example.com/ destinations=[93.184.216.34:443]"
            ),
            CheckResult::Allowed
        );

        let allow = PermissionConfig {
            js_fetch: Some(ToolPerm::Simple(Action::Allow)),
            ..PermissionConfig::default()
        };
        assert_eq!(
            new_checker(SecurityMode::Standard, allow).check(
                "js/fetch",
                "https://example.com/ destinations=[93.184.216.34:443]"
            ),
            CheckResult::Allowed
        );
        let deny = PermissionConfig {
            js_fetch: Some(ToolPerm::Simple(Action::Deny)),
            ..PermissionConfig::default()
        };
        assert!(matches!(
            new_checker(SecurityMode::Yolo, deny).check(
                "js/fetch",
                "https://example.com/ destinations=[93.184.216.34:443]"
            ),
            CheckResult::Denied(_)
        ));
    }

    #[test]
    fn cwd_cache_invalidates_on_rebind_working_dir() {
        let temp = TempDir::new();
        let workspace_a = temp.0.join("workspace_a");
        let workspace_b = temp.0.join("workspace_b");
        let external = temp.0.join("external");
        std::fs::create_dir_all(&workspace_a).unwrap();
        std::fs::create_dir_all(&workspace_b).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        let mut checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(workspace_a.clone()),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission fixture");

        let external_path = external.join("file.txt").to_string_lossy().to_string();

        assert!(
            checker.is_external_path(&external_path),
            "path outside workspace_a should be external"
        );

        checker.rebind_working_dir(&external).unwrap();

        assert!(
            !checker.is_external_path("file.txt"),
            "same file is now internal after rebind to external directory"
        );

        checker.rebind_working_dir(&workspace_b).unwrap();

        let external_from_b = external.join("file.txt").to_string_lossy().to_string();
        assert!(
            checker.is_external_path(&external_from_b),
            "path is external again after rebind to workspace_b"
        );
    }

    #[test]
    fn is_external_path_cache_produces_same_answer_as_live_resolution() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        let external = temp.0.join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        let mut checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(workspace.clone()),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission fixture");

        let test_paths = [
            workspace.join("inside.txt").to_string_lossy().to_string(),
            external.join("outside.txt").to_string_lossy().to_string(),
            workspace
                .join("nonexistent/file.txt")
                .to_string_lossy()
                .to_string(),
            "relative_inside.txt".to_string(),
        ];

        let cached_results: Vec<bool> = test_paths
            .iter()
            .map(|p| checker.is_external_path(p))
            .collect();

        checker.cached_resolved_cwd = None;

        let live_results: Vec<bool> = test_paths
            .iter()
            .map(|p| checker.is_external_path(p))
            .collect();

        assert_eq!(
            cached_results, live_results,
            "cache must produce identical results to live resolution"
        );
    }

    #[test]
    fn session_allowlist_entries_match_before_and_after_optimization() {
        let temp = TempDir::new();
        let workspace = temp.0.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let mut checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(workspace.clone()),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission fixture");

        let entries = vec![
            ("read".to_string(), "src/**".to_string()),
            ("write".to_string(), "docs/**".to_string()),
            ("read".to_string(), "README.md".to_string()),
        ];

        checker.load_session_allowlist(&entries);

        assert!(
            checker.session_allowlist.len() >= 3,
            "at least all input entries should be in allowlist"
        );

        let read_entries: Vec<_> = checker
            .session_allowlist
            .iter()
            .filter(|(t, _)| t == "read")
            .collect();

        assert!(
            read_entries.len() >= 2,
            "should have at least 2 read entries (src/** and README.md)"
        );

        assert!(
            read_entries.iter().any(|(_, p)| p.matches("README.md")
                || p.matches(&workspace.join("README.md").to_string_lossy())),
            "README.md entry should match against both relative and absolute paths"
        );

        let write_entries: Vec<_> = checker
            .session_allowlist
            .iter()
            .filter(|(t, _)| t == "write")
            .collect();

        assert!(
            !write_entries.is_empty(),
            "should have write entry for docs/**"
        );
    }
}
