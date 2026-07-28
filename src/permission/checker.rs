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
    session_allowlist: Vec<(String, Pattern)>,
    last_call: Option<(String, String)>,
    consecutive_repeat_count: usize,
    mode: SecurityMode,
    user_mode: SecurityMode,
    permission_modes: Vec<SecurityMode>,
    allow_all_mcp_calls: bool,
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
    fn compile_config(
        config: &PermissionConfig,
        is_regex: bool,
    ) -> HashMap<String, Vec<(Pattern, Action)>> {
        let mut rules: HashMap<String, Vec<(Pattern, Action)>> = HashMap::new();
        for (tool_name, tool_perm) in [
            ("bash", &config.bash),
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
                        Pattern::new_regex(".*")
                    } else {
                        Pattern::new("*")
                    };
                    entries.push((pat, *action));
                }
                ToolPerm::Granular(map) => {
                    for (pat, action) in map {
                        let pat = if is_regex {
                            Pattern::new_regex(pat)
                        } else {
                            Pattern::new(pat)
                        };
                        entries.push((pat, *action));
                    }
                }
            }
            rules.insert(tool_name.to_string(), entries.clone());
            if let Some(alias) = js_file_tool_alias(tool_name) {
                rules.insert(alias.to_string(), entries);
            }
        }
        rules
    }

    pub fn new(
        configs: &PermissionConfigs,
        mode: SecurityMode,
        working_dir: Option<std::path::PathBuf>,
        permission_modes: Option<Vec<String>>,
    ) -> Self {
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

        let mut rules = Self::compile_config(&configs.glob, false);
        let regex_rules = Self::compile_config(&configs.regex, true);
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
                    let aliases = [Some(tool.as_str()), js_file_tool_alias(tool)];
                    for alias in aliases.into_iter().flatten() {
                        let entry = rules.entry(alias.to_string()).or_default();
                        for pat in patterns {
                            entry.push((Pattern::new(pat), action));
                        }
                    }
                }
            }
        }

        merge_entries(&mut rules, &configs.glob.allow_entries, Action::Allow);
        merge_entries(&mut rules, &configs.glob.ask_entries, Action::Ask);
        merge_entries(&mut rules, &configs.glob.deny_entries, Action::Deny);

        if !rules.contains_key("bash") {
            let mut defaults = Vec::new();
            for (pat, action) in crate::permission::default_bash_rules() {
                defaults.push((Pattern::new(pat), action));
            }
            rules.insert("bash".to_string(), defaults);
        }

        for (tool, regex) in crate::permission::default_deny_regex_rules() {
            rules
                .entry(tool.to_string())
                .or_default()
                .push((Pattern::new_regex(regex), Action::Deny));
        }

        let ext_dir_rules = configs
            .glob
            .external_directory
            .as_ref()
            .map(|map| {
                map.iter()
                    .map(|(pat, action)| (Pattern::new(&resolve_glob_pattern(pat)), *action))
                    .collect()
            })
            .unwrap_or_default();

        let working_dir = working_dir
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
            .to_string_lossy()
            .to_string();

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

        PermissionChecker {
            rules,
            default_action,
            ext_dir_rules,
            doom_loop_action,
            working_dir,
            session_allowlist: Vec::new(),
            last_call: None,
            consecutive_repeat_count: 0,
            mode,
            user_mode: mode,
            permission_modes: resolved_modes,
            allow_all_mcp_calls: false,
            #[cfg(feature = "hooks")]
            pending_forced_ask: None,
            #[cfg(feature = "hooks")]
            pending_one_shot_allow: None,
        }
    }

    /// Forces the next `check`/`check_path` call for `tool` to `Ask`,
    /// regardless of permission mode. Consumed after that one call. Set by a
    /// hook `ask` verdict; never overrides a deny rule (checked first).
    #[cfg(feature = "hooks")]
    pub fn force_ask_once(&mut self, tool: String) {
        self.pending_forced_ask = Some(tool);
    }

    /// Suppresses the interactive prompt for the next `check`/`check_path`
    /// call for `tool`. Consumed after that one call. Set by a hook `allow`
    /// verdict; never overrides a deny rule (checked first).
    #[cfg(feature = "hooks")]
    pub fn allow_once(&mut self, tool: String) {
        self.pending_one_shot_allow = Some(tool);
    }

    fn apply_rules(&self) -> bool {
        self.permission_modes.contains(&self.mode) || self.mode == SecurityMode::Yolo
    }

    fn is_read_tool(&self, tool: &str) -> bool {
        matches!(
            tool,
            "read" | "js/read_file" | "grep" | "find_files" | "list_dir" | "task"
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
            SecurityMode::Standard => base.unwrap_or_else(|| {
                if tool == "bash" {
                    // Bash scripts are opaque permission keys. An unmatched
                    // script must never inherit a permissive default.
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
    ) -> Action {
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
                        && is_plan_file(abs_path))
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
                if matched.is_empty() && self.is_path_tool(tool) && !self.is_external_path(abs_path)
                {
                    Action::Allow
                } else if matched.is_empty()
                    && a == Action::Allow
                    && self.is_external_path(abs_path)
                {
                    self.match_ext_dir(abs_path).unwrap_or(Action::Ask)
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
                if action == Action::Allow {
                    let count = self.count_doom_loop();
                    return CheckResult::allowed_with_coaching(tool, doom_key, count);
                }
                match self.doom_loop_action {
                    Action::Deny => {
                        tracing::info!("perm doom-loop blocked: tool={}", tool);
                        return CheckResult::Denied(
                            "Doom loop: repeated identical tool call".to_string(),
                        );
                    }
                    Action::Ask => {
                        tracing::info!("perm doom-loop ask: tool={}", tool);
                        return CheckResult::Ask;
                    }
                    Action::Allow => {}
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
        self.check_inner(tool, input, false)
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
        self.check_inner("mcp_tool", input, read_only_exempt)
    }

    fn check_inner(&mut self, tool: &str, input: &str, mcp_read_only_exempt: bool) -> CheckResult {
        tracing::debug!("perm check: tool={}, input_len={}", tool, input.len());
        if tool == "todo_write" {
            return CheckResult::Allowed;
        }
        // Deny rules are the security baseline — evaluate before the session
        // allowlist and allow_all_mcp_calls so neither can bypass a deny.
        if self.matches_deny_rule(tool, &[input]) {
            return CheckResult::Denied("Blocked by deny rule".to_string());
        }
        #[cfg(feature = "hooks")]
        if let Some(result) = self.take_pending_one_shot(tool) {
            return result;
        }
        if self.allow_all_mcp_calls && tool == "mcp_tool" {
            return CheckResult::Allowed;
        }
        if self.is_session_allowed(tool, input) {
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
                let matches = if tool == "bash" && *action == Action::Allow {
                    // Model B: allow only the exact, complete script. Ask and
                    // deny rules remain pattern-based so broad safeguards keep
                    // working, but globs/regexes cannot widen Bash execution.
                    pattern.original == input
                } else {
                    pattern.matches(input)
                };
                if matches {
                    matched.push(*action);
                }
            }
        }

        let action = self.resolve_check_action(tool, &matched);
        self.doom_loop_check(tool, input, action)
    }

    pub fn check_path(&mut self, tool: &str, path: &str) -> CheckResult {
        tracing::debug!("perm check path: tool={}, path={}", tool, path);
        if tool == "todo_write" {
            return CheckResult::Allowed;
        }

        let expanded = crate::fs::expand_tilde(path);
        let abs_path = resolve_absolute(&expanded, &self.working_dir);

        // Deny rules first — security baseline, cannot be bypassed.
        if self.matches_deny_rule(tool, &[&abs_path, &expanded]) {
            return CheckResult::Denied("Blocked by deny rule".to_string());
        }
        #[cfg(feature = "hooks")]
        if let Some(result) = self.take_pending_one_shot(tool) {
            return result;
        }
        if self.is_session_allowed(tool, &expanded) || self.is_session_allowed(tool, &abs_path) {
            return CheckResult::Allowed;
        }

        let mut matched: SmallVec<[Action; 4]> = SmallVec::new();
        if self.apply_rules()
            && let Some(rules) = self.rules.get(tool)
        {
            for (pattern, action) in rules {
                if pattern.matches(&abs_path) || pattern.matches(&expanded) {
                    matched.push(*action);
                }
            }
        }

        let action = self.resolve_path_action(tool, &matched, &abs_path);
        self.doom_loop_check(tool, &expanded, action)
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
                if *action == Action::Deny && inputs.iter().any(|inp| pattern.matches(inp)) {
                    return true;
                }
            }
        }
        false
    }

    fn is_session_allowed(&self, tool: &str, input: &str) -> bool {
        for (allowed_tool, pattern) in &self.session_allowlist {
            let matches = if tool == "bash" {
                pattern.original == input
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
        let pattern = Pattern::new(pattern_str);
        self.session_allowlist.push((tool.clone(), pattern));
        if self.is_path_tool(&tool) {
            let expanded = crate::fs::expand_tilde(pattern_str);
            let abs = resolve_absolute(&expanded, &self.working_dir);
            if abs != expanded {
                self.session_allowlist.push((tool, Pattern::new(&abs)));
            }
        }
    }

    pub fn load_session_allowlist(&mut self, entries: &[(String, String)]) {
        for (tool, pat) in entries {
            let pattern = Pattern::new(pat);
            self.session_allowlist.push((tool.clone(), pattern));
            if self.is_path_tool(tool) {
                let expanded = crate::fs::expand_tilde(pat);
                let abs = resolve_absolute(&expanded, &self.working_dir);
                if abs != expanded {
                    self.session_allowlist
                        .push((tool.clone(), Pattern::new(&abs)));
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
        matches!(
            tool,
            "read" | "write" | "edit" | "list_dir" | "js/read_file" | "js/write_file"
        )
    }

    fn is_external_path(&self, path_str: &str) -> bool {
        let p = Path::new(path_str);
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(&self.working_dir).join(p)
        };
        let cwd = Path::new(&self.working_dir);
        let Some(normalized) = resolve_path_allow_missing(&p) else {
            return true;
        };
        let Some(normalized_cwd) = resolve_path_allow_missing(cwd) else {
            return true;
        };
        !normalized.starts_with(normalized_cwd)
    }

    fn match_ext_dir(&self, path_str: &str) -> Option<Action> {
        let resolved = resolve_path_allow_missing(Path::new(path_str))?;
        let resolved = resolved.to_string_lossy();
        for (pattern, action) in &self.ext_dir_rules {
            if pattern.matches(&resolved) {
                return Some(*action);
            }
        }
        None
    }

    /// Feeds a hook-denied call into doom-loop detection. A hook deny never
    /// reaches `check`/`check_path`, so without this a denied call could
    /// retry forever invisibly to doom detection.
    #[cfg(feature = "hooks")]
    pub fn record_blocked(&mut self, tool: &str, input: &str) {
        self.track_doom_loop(tool, input);
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

fn js_file_tool_alias(tool: &str) -> Option<&'static str> {
    match tool {
        "read" => Some("js/read_file"),
        "write" => Some("js/write_file"),
        _ => None,
    }
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
            let path = std::env::temp_dir().join(format!(
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
        );

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
        );

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

        let default_checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(workspace),
            Some(vec!["standard".to_string()]),
        );
        assert!(
            default_checker.is_external_path(&sibling.join("outside.txt").to_string_lossy()),
            "sibling-prefix path must remain outside the workspace"
        );
    }
}
