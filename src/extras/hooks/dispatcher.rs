use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use regex::Regex;

use super::channel::{ChannelResult, interpret_hook_output};
use super::envelope::{EventFields, build_envelope};
use super::normalize::canonical_tool_name;
use super::settings::{HookHandler, HooksConfig};
use super::subprocess::{
    HookOutput, HookPolicy, HookStatus, run_hook_with_policy_at_root, run_shell_condition_at_root,
};
use super::{Decision, HookCtx, PreDecision, Verdict};

/// Default per-hook timeout when a handler doesn't declare one.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const REDACTION_MARKER: &str = "[REDACTED]";

enum CompiledMatcher {
    All,
    Names(HashSet<String>),
    Regex(Regex),
}

impl CompiledMatcher {
    fn compile(matcher: &Option<String>) -> Result<Self, String> {
        match matcher.as_deref() {
            None | Some("") | Some("*") => Ok(CompiledMatcher::All),
            Some(s) if is_plain_name_list(s) => {
                let names = s
                    .split(['|', ','])
                    .map(|n| canonical_tool_name(n.trim()))
                    .collect();
                Ok(CompiledMatcher::Names(names))
            }
            Some(s) => Regex::new(s)
                .map(CompiledMatcher::Regex)
                .map_err(|e| format!("hooks: invalid matcher regex `{s}`: {e}")),
        }
    }

    fn matches(&self, name: &str) -> bool {
        match self {
            CompiledMatcher::All => true,
            CompiledMatcher::Names(set) => set.contains(name),
            CompiledMatcher::Regex(re) => re.is_match(name),
        }
    }
}

fn is_plain_name_list(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '|' || c == ',' || c.is_whitespace())
}

struct MatcherEntry {
    matcher: CompiledMatcher,
    handlers: Vec<HookHandler>,
}

#[derive(Clone)]
struct ValidatedExecutionRoot {
    configured: PathBuf,
    canonical: PathBuf,
    identity: crate::fs::CheckedMetadata,
}

impl ValidatedExecutionRoot {
    fn capture(path: &Path) -> Result<Self, String> {
        let canonical = std::fs::canonicalize(path).map_err(|error| {
            format!(
                "hooks: execution workspace '{}' is unavailable: {error}",
                path.display()
            )
        })?;
        if canonical.parent().is_none() {
            return Err("hooks: execution workspace cannot be the filesystem root".to_string());
        }
        let identity = crate::fs::checked_path_metadata(&canonical).map_err(|error| {
            format!(
                "hooks: execution workspace '{}' cannot be identified: {error}",
                canonical.display()
            )
        })?;
        if !identity.is_dir() {
            return Err("hooks: execution workspace is not a directory".to_string());
        }
        Ok(Self {
            configured: path.to_path_buf(),
            canonical,
            identity,
        })
    }

    fn revalidate(&self) -> Result<&Path, String> {
        let canonical = std::fs::canonicalize(&self.configured).map_err(|error| {
            format!(
                "hooks: execution workspace '{}' is unavailable: {error}",
                self.configured.display()
            )
        })?;
        if canonical != self.canonical {
            return Err("hooks: execution workspace changed after selection".to_string());
        }
        let current = crate::fs::checked_path_metadata(&canonical).map_err(|error| {
            format!(
                "hooks: execution workspace '{}' cannot be revalidated: {error}",
                canonical.display()
            )
        })?;
        crate::fs::ensure_same_file(&canonical, &self.identity, &current).map_err(|_| {
            "hooks: execution workspace identity changed after selection".to_string()
        })?;
        if !current.is_dir() {
            return Err("hooks: execution workspace is no longer a directory".to_string());
        }
        Ok(&self.canonical)
    }
}

enum ExecutionRootBinding {
    /// Unit-test and explicitly unbound dispatchers validate each supplied context.
    Unbound,
    Valid(ValidatedExecutionRoot),
    /// A failed rebind must disable execution instead of retaining stale authority.
    Invalid(String),
}

struct ExecutionRootState {
    generation: u64,
    binding: ExecutionRootBinding,
}

/// A dispatch-local proof that remains bound to the exact selected directory
/// identity and selection generation until each child has been created.
#[derive(Clone)]
pub(crate) struct HookExecutionRootLease {
    state: Arc<RwLock<ExecutionRootState>>,
    generation: u64,
    root: ValidatedExecutionRoot,
}

impl HookExecutionRootLease {
    fn revalidate_locked<'a>(&'a self, state: &ExecutionRootState) -> Result<&'a Path, String> {
        if state.generation != self.generation {
            return Err("hooks: execution workspace changed during dispatch".to_string());
        }
        match &state.binding {
            ExecutionRootBinding::Invalid(error) => return Err(error.clone()),
            ExecutionRootBinding::Unbound | ExecutionRootBinding::Valid(_) => {}
        }
        self.root.revalidate()
    }

    /// Revalidates the selected path and keeps rebinding serialized through
    /// synchronous child creation. A completed rebind invalidates this lease;
    /// one already holding the read lock linearizes its spawn before rebind.
    pub(crate) fn with_validated_root<T>(
        &self,
        expected_project_dir: &Path,
        operation: impl FnOnce() -> T,
    ) -> Result<T, String> {
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let canonical = self.revalidate_locked(&state)?;
        if canonical != expected_project_dir {
            return Err("hooks: execution workspace changed before child creation".to_string());
        }
        Ok(operation())
    }
}

/// The rig-free dispatcher seam: accepts only strings/JSON and returns only
/// zerostack-owned `Decision`/`PreDecision` values. See hook-dispatch spec's
/// "rig-free dispatcher seam" requirement.
pub(crate) struct HookDispatcher {
    events: HashMap<String, Vec<MatcherEntry>>,
    sandbox_backend: String,
    /// Immutable root used to hash and approve project-local hook bindings.
    /// Worktree selection never changes this trust decision.
    _trust_binding_root: Option<PathBuf>,
    /// Separately validated workspace used by envelopes and hook children.
    execution_root: Arc<RwLock<ExecutionRootState>>,
    /// Full handler bindings with `once: true` that have already run, keyed by
    /// event so distinct argv, conditions, environments, and trust policies
    /// never consume one another's once slot.
    once_ran: Arc<Mutex<HashSet<(String, HookHandler)>>>,
}

impl HookDispatcher {
    pub(crate) fn from_config(config: &HooksConfig) -> Result<Self, String> {
        let backend = if cfg!(target_os = "macos") {
            "seatbelt"
        } else {
            "bwrap"
        };
        Self::from_config_with_backend(config, backend)
    }

    pub(crate) fn from_config_with_backend(
        config: &HooksConfig,
        sandbox_backend: &str,
    ) -> Result<Self, String> {
        Self::from_config_with_backend_and_optional_root(config, sandbox_backend, None)
    }

    pub(crate) fn from_config_with_backend_and_root(
        config: &HooksConfig,
        sandbox_backend: &str,
        project_root: &Path,
    ) -> Result<Self, String> {
        Self::from_config_with_backend_and_optional_root(
            config,
            sandbox_backend,
            Some(project_root.to_path_buf()),
        )
    }

    fn from_config_with_backend_and_optional_root(
        config: &HooksConfig,
        sandbox_backend: &str,
        project_root: Option<PathBuf>,
    ) -> Result<Self, String> {
        let mut events = HashMap::new();
        for (event, groups) in config {
            let mut entries = Vec::with_capacity(groups.len());
            for group in groups {
                let matcher = CompiledMatcher::compile(&group.matcher)?;
                entries.push(MatcherEntry {
                    matcher,
                    handlers: group.hooks.clone(),
                });
            }
            events.insert(event.clone(), entries);
        }
        let execution_root = match project_root.as_deref() {
            Some(root) => match ValidatedExecutionRoot::capture(root) {
                Ok(root) => ExecutionRootBinding::Valid(root),
                Err(error) => ExecutionRootBinding::Invalid(error),
            },
            None => ExecutionRootBinding::Unbound,
        };
        Ok(Self {
            events,
            sandbox_backend: sandbox_backend.to_string(),
            _trust_binding_root: project_root,
            execution_root: Arc::new(RwLock::new(ExecutionRootState {
                generation: 0,
                binding: execution_root,
            })),
            once_ran: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// Rebind hook execution after a validated UI/session workspace switch.
    /// Failure is sticky and fail-closed until a later valid rebind succeeds.
    pub(crate) fn rebind_execution_root(&self, workspace: &Path) -> Result<(), String> {
        let mut state = self
            .execution_root
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(next_generation) = state.generation.checked_add(1) else {
            let error = "hooks: execution workspace generation exhausted".to_string();
            state.binding = ExecutionRootBinding::Invalid(error.clone());
            return Err(error);
        };
        state.generation = next_generation;
        state.binding = ExecutionRootBinding::Invalid(
            "hooks: execution workspace revalidation is incomplete".to_string(),
        );
        match ValidatedExecutionRoot::capture(workspace) {
            Ok(root) => {
                state.binding = ExecutionRootBinding::Valid(root);
                Ok(())
            }
            Err(error) => {
                state.binding = ExecutionRootBinding::Invalid(error.clone());
                Err(error)
            }
        }
    }

    fn policy_context(&self, ctx: &HookCtx) -> Result<(HookCtx, HookExecutionRootLease), String> {
        let mut ctx = ctx.clone();
        let state = self
            .execution_root
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = match &state.binding {
            ExecutionRootBinding::Unbound => ValidatedExecutionRoot::capture(Path::new(&ctx.cwd))?,
            ExecutionRootBinding::Valid(root) => root.clone(),
            ExecutionRootBinding::Invalid(error) => return Err(error.clone()),
        };
        let lease = HookExecutionRootLease {
            state: Arc::clone(&self.execution_root),
            generation: state.generation,
            root,
        };
        let canonical = lease.revalidate_locked(&state)?.to_path_buf();
        ctx.cwd = canonical.to_string_lossy().into_owned();
        Ok((ctx, lease))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.events
            .values()
            .all(|entries| entries.iter().all(|entry| entry.handlers.is_empty()))
    }

    /// True when any `PreToolUse`/`PostToolUse`/`PostToolUseFailure` handler is
    /// registered. Lets the tool decorator skip per-call context building (a
    /// `current_dir` syscall + permission lock) when only lifecycle hooks are
    /// configured, restoring the zero-cost invariant for that case.
    pub(crate) fn has_tool_hooks(&self) -> bool {
        ["PreToolUse", "PostToolUse", "PostToolUseFailure"]
            .iter()
            .any(|event| {
                self.events
                    .get(*event)
                    .is_some_and(|entries| entries.iter().any(|entry| !entry.handlers.is_empty()))
            })
    }

    /// Configured events with their total handler count (across all matcher
    /// groups), sorted by event name, omitting events with no handlers. For
    /// display (`/hooks`), not dispatch.
    pub(crate) fn summary(&self) -> Vec<(String, usize)> {
        let mut result: Vec<(String, usize)> = self
            .events
            .iter()
            .map(|(event, entries)| {
                let count: usize = entries.iter().map(|e| e.handlers.len()).sum();
                (event.clone(), count)
            })
            .filter(|(_, count)| *count > 0)
            .collect();
        result.sort();
        result
    }

    /// Handlers matching `event`/`canonical_tool_name`, in declared order.
    /// Exact duplicate handler definitions within one matcher group are
    /// deduplicated (first occurrence wins); separate policy sources/groups
    /// remain independent guard rails.
    pub(crate) fn handlers_for(&self, event: &str, canonical_tool_name: &str) -> Vec<&HookHandler> {
        let Some(entries) = self.events.get(event) else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for entry in entries {
            if !entry.matcher.matches(canonical_tool_name) {
                continue;
            }
            let mut seen = HashSet::new();
            for handler in &entry.handlers {
                if !seen.insert(handler) {
                    continue;
                }
                result.push(handler);
            }
        }
        result
    }

    /// Generic dispatch for non-permission lifecycle events. Returns
    /// `Decision::Continue` after only an index lookup when nothing matches
    /// (the zero-cost invariant).
    pub(crate) async fn dispatch(
        &self,
        event: &str,
        canonical_tool_name: Option<&str>,
        ctx: &HookCtx,
        fields: EventFields,
    ) -> Decision {
        let handlers = self.handlers_for(event, canonical_tool_name.unwrap_or(""));
        if handlers.is_empty() {
            return Decision::Continue;
        }
        let (ctx, execution_root) = match self.policy_context(ctx) {
            Ok(bound) => bound,
            Err(error) => {
                tracing::warn!(event, error = %error, "hooks: refusing dispatch in invalid workspace");
                return Decision::Block { reason: error };
            }
        };
        let envelope = build_envelope(&ctx, event, fields);
        let outputs = self
            .run_handlers(event, &handlers, &envelope, &ctx.cwd, &execution_root)
            .await;
        merge_decisions(event, &outputs)
    }

    /// Dispatches `PreToolUse`: the only blockable-by-default tool event.
    pub(crate) async fn dispatch_pre_tool_use(
        &self,
        ctx: &HookCtx,
        tool_name: &str,
        tool_input: serde_json::Value,
    ) -> PreDecision {
        let canonical = canonical_tool_name(tool_name);
        let handlers = self.handlers_for("PreToolUse", &canonical);
        if handlers.is_empty() {
            return PreDecision {
                verdict: Verdict::Defer,
                reason: None,
                updated_input: None,
            };
        }
        let (ctx, execution_root) = match self.policy_context(ctx) {
            Ok(bound) => bound,
            Err(error) => {
                tracing::warn!(error = %error, "hooks: refusing PreToolUse dispatch in invalid workspace");
                return PreDecision {
                    verdict: Verdict::Deny,
                    reason: Some(error),
                    updated_input: None,
                };
            }
        };
        let envelope = build_envelope(
            &ctx,
            "PreToolUse",
            EventFields::PreToolUse {
                tool_name: canonical,
                tool_input: tool_input.clone(),
            },
        );
        let outputs = self
            .run_handlers(
                "PreToolUse",
                &handlers,
                &envelope,
                &ctx.cwd,
                &execution_root,
            )
            .await;
        let parts: Vec<PreDecisionPart> = outputs.iter().map(parse_pre_decision_part).collect();
        merge_pre_decisions(&tool_input, &parts)
    }

    /// Dispatches `PostToolUse`: may redact exact strings from the
    /// model-visible result, but cannot replace it with hook-authored content.
    pub(crate) async fn dispatch_post_tool_use(
        &self,
        ctx: &HookCtx,
        tool_name: &str,
        tool_input: serde_json::Value,
        tool_response: &str,
    ) -> Decision {
        let canonical = canonical_tool_name(tool_name);
        let handlers = self.handlers_for("PostToolUse", &canonical);
        if handlers.is_empty() {
            return Decision::Continue;
        }
        let (ctx, execution_root) = match self.policy_context(ctx) {
            Ok(bound) => bound,
            Err(error) => {
                tracing::warn!(error = %error, "hooks: skipping PostToolUse dispatch in invalid workspace");
                return Decision::Continue;
            }
        };
        let envelope = build_envelope(
            &ctx,
            "PostToolUse",
            EventFields::PostToolUse {
                tool_name: canonical,
                tool_input,
                tool_response: tool_response.to_string(),
            },
        );
        let outputs = self
            .run_handlers(
                "PostToolUse",
                &handlers,
                &envelope,
                &ctx.cwd,
                &execution_root,
            )
            .await;
        merge_post_tool_use_decisions(&outputs, tool_response)
    }

    /// Dispatches `PostToolUseFailure`: observation only, never blockable.
    pub(crate) async fn dispatch_post_tool_use_failure(
        &self,
        ctx: &HookCtx,
        tool_name: &str,
        tool_input: serde_json::Value,
        error: &str,
    ) {
        let canonical = canonical_tool_name(tool_name);
        let handlers = self.handlers_for("PostToolUseFailure", &canonical);
        if handlers.is_empty() {
            return;
        }
        let (ctx, execution_root) = match self.policy_context(ctx) {
            Ok(bound) => bound,
            Err(error) => {
                tracing::warn!(error = %error, "hooks: skipping PostToolUseFailure dispatch in invalid workspace");
                return;
            }
        };
        let envelope = build_envelope(
            &ctx,
            "PostToolUseFailure",
            EventFields::PostToolUseFailure {
                tool_name: canonical,
                tool_input,
                error: error.to_string(),
            },
        );
        let _ = self
            .run_handlers(
                "PostToolUseFailure",
                &handlers,
                &envelope,
                &ctx.cwd,
                &execution_root,
            )
            .await;
    }

    /// Runs matching handlers: skips a handler already consumed by `once`,
    /// evaluates `if` (fail-closed: any parse/spawn/timeout failure runs the
    /// handler anyway, with a warning), then spawns the command itself.
    async fn run_handlers(
        &self,
        event: &str,
        handlers: &[&HookHandler],
        envelope: &serde_json::Value,
        project_dir: &str,
        execution_root: &HookExecutionRootLease,
    ) -> Vec<HookOutput> {
        let stdin = serde_json::to_vec(envelope).unwrap_or_default();
        let mut futures = Vec::new();
        for handler in handlers {
            let Some(command) = handler.command.clone() else {
                continue;
            };

            if handler.is_async {
                let handler = (*handler).clone();
                let event = event.to_string();
                let stdin = stdin.clone();
                let project_dir = project_dir.to_string();
                let execution_root = execution_root.clone();
                let sandbox_backend = self.sandbox_backend.clone();
                let once_ran = Arc::clone(&self.once_ran);
                std::mem::drop(crate::agent::runner::spawn_async_scoped(async move {
                    let policy =
                        HookPolicy::new(handler.trust, &sandbox_backend, handler.env.clone());
                    if let Some(condition) = &handler.condition {
                        let cond_timeout = std::time::Duration::from_secs(
                            handler.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS),
                        );
                        let cond_output = run_shell_condition_at_root(
                            condition,
                            &stdin,
                            cond_timeout,
                            &project_dir,
                            &policy,
                            &execution_root,
                        )
                        .await;
                        audit_hook_outcome(
                            &event,
                            &command,
                            "condition",
                            handler.trust,
                            &cond_output,
                        );
                        match cond_output.status {
                            HookStatus::TimedOut => tracing::warn!(
                                "hooks: `if` condition for {command:?} timed out; failing closed (running the handler)"
                            ),
                            HookStatus::OutputLimitExceeded(limit) => tracing::warn!(
                                "hooks: `if` condition for {command:?} exceeded its {limit:?} output limit; failing closed (running the handler)"
                            ),
                            HookStatus::Completed
                            | HookStatus::Failed
                            | HookStatus::PolicyDenied => match cond_output.exit_code {
                                Some(0) => {}
                                Some(_) => return,
                                None => tracing::warn!(
                                    "hooks: `if` condition for {command:?} could not be completed; failing closed (running the handler)"
                                ),
                            },
                        }
                    }

                    let once_key = handler.once.then(|| (event.clone(), handler.clone()));
                    let Some(mut once_reservation) = OnceReservation::reserve(once_ran, once_key)
                    else {
                        return;
                    };
                    let diagnostics = policy.diagnostics();
                    tracing::info!(
                        event,
                        command = command.as_str(),
                        trust = ?handler.trust,
                        containment = diagnostics.containment,
                        filesystem = diagnostics.filesystem,
                        network = diagnostics.network,
                        "hooks: applying subprocess policy"
                    );
                    let timeout = std::time::Duration::from_secs(
                        handler.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS),
                    );
                    let output = run_hook_with_policy_at_root(
                        &command,
                        handler.args.as_deref(),
                        &stdin,
                        timeout,
                        &project_dir,
                        &policy,
                        &execution_root,
                    )
                    .await;
                    once_reservation.consume_if_started(&output);
                    audit_hook_outcome(&event, &command, "handler", handler.trust, &output);
                }));
                continue;
            }

            if let Some(condition) = &handler.condition {
                let policy =
                    HookPolicy::new(handler.trust, &self.sandbox_backend, handler.env.clone());
                let cond_timeout =
                    std::time::Duration::from_secs(handler.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));
                let cond_output = run_shell_condition_at_root(
                    condition,
                    &stdin,
                    cond_timeout,
                    project_dir,
                    &policy,
                    execution_root,
                )
                .await;
                audit_hook_outcome(event, &command, "condition", handler.trust, &cond_output);
                match cond_output.status {
                    HookStatus::TimedOut => {
                        tracing::warn!(
                            "hooks: `if` condition for {command:?} timed out; failing closed (running the handler)"
                        );
                    }
                    HookStatus::OutputLimitExceeded(limit) => {
                        tracing::warn!(
                            "hooks: `if` condition for {command:?} exceeded its {limit:?} output limit; failing closed (running the handler)"
                        );
                    }
                    HookStatus::Completed | HookStatus::Failed | HookStatus::PolicyDenied => {
                        match cond_output.exit_code {
                            Some(0) => {}
                            Some(_) => continue,
                            None => {
                                tracing::warn!(
                                    "hooks: `if` condition for {command:?} could not be completed; failing closed (running the handler)"
                                );
                            }
                        }
                    }
                }
            }

            // A false condition never consumes `once`. Reserve immediately
            // before launch so concurrent dispatches cannot run the binding
            // twice, then release only if no child/wrapper was started.
            let once_key = handler
                .once
                .then(|| (event.to_string(), (*handler).clone()));
            let Some(mut once_reservation) =
                OnceReservation::reserve(Arc::clone(&self.once_ran), once_key)
            else {
                continue;
            };

            let timeout =
                std::time::Duration::from_secs(handler.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));
            let stdin = stdin.clone();
            let project_dir = project_dir.to_string();
            let execution_root = execution_root.clone();
            let args = handler.args.clone();
            let policy = HookPolicy::new(handler.trust, &self.sandbox_backend, handler.env.clone());
            let diagnostics = policy.diagnostics();
            let trust = handler.trust;
            let audit_event = event.to_string();
            tracing::info!(
                event,
                command = command.as_str(),
                trust = ?handler.trust,
                containment = diagnostics.containment,
                filesystem = diagnostics.filesystem,
                network = diagnostics.network,
                "hooks: applying subprocess policy"
            );
            futures.push(async move {
                let output = run_hook_with_policy_at_root(
                    &command,
                    args.as_deref(),
                    &stdin,
                    timeout,
                    &project_dir,
                    &policy,
                    &execution_root,
                )
                .await;
                once_reservation.consume_if_started(&output);
                audit_hook_outcome(&audit_event, &command, "handler", trust, &output);
                output
            });
        }
        futures::future::join_all(futures).await
    }
}

struct OnceReservation {
    once_ran: Arc<Mutex<HashSet<(String, HookHandler)>>>,
    key: Option<(String, HookHandler)>,
}

impl OnceReservation {
    fn reserve(
        once_ran: Arc<Mutex<HashSet<(String, HookHandler)>>>,
        key: Option<(String, HookHandler)>,
    ) -> Option<Self> {
        if let Some(key) = &key
            && !once_ran
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone())
        {
            return None;
        }
        Some(Self { once_ran, key })
    }

    fn consume_if_started(&mut self, output: &HookOutput) {
        if output.started {
            self.key = None;
        }
    }
}

impl Drop for OnceReservation {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            self.once_ran
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(key);
        }
    }
}

fn audit_hook_outcome(
    event: &str,
    command: &str,
    role: &str,
    trust: super::settings::HookTrust,
    output: &HookOutput,
) {
    tracing::info!(
        event,
        command,
        role,
        trust = ?trust,
        status = ?output.status,
        containment = output.diagnostics.containment,
        environment = output.diagnostics.environment,
        filesystem = output.diagnostics.filesystem,
        network = output.diagnostics.network,
        "hooks: subprocess policy outcome"
    );
}

struct PreDecisionPart {
    verdict: Verdict,
    reason: Option<String>,
    updated_input: Option<serde_json::Value>,
}

fn parse_pre_decision_part(output: &HookOutput) -> PreDecisionPart {
    match interpret_hook_output(output) {
        ChannelResult::Block { stderr } => PreDecisionPart {
            verdict: Verdict::Deny,
            reason: Some(stderr),
            updated_input: None,
        },
        ChannelResult::NoObjection { json: Some(value) } => {
            let verdict = match value.get("permissionDecision").and_then(|v| v.as_str()) {
                Some("deny") => Verdict::Deny,
                Some("ask") => Verdict::Ask,
                Some("allow") => Verdict::Allow,
                _ => Verdict::Defer,
            };
            let reason = value
                .get("reason")
                .and_then(|v| v.as_str())
                .map(String::from);
            let updated_input = value.get("updatedInput").cloned();
            PreDecisionPart {
                verdict,
                reason,
                updated_input,
            }
        }
        ChannelResult::NoObjection { json: None } => PreDecisionPart {
            verdict: Verdict::Defer,
            reason: None,
            updated_input: None,
        },
        ChannelResult::Error { exit_code, .. } => {
            // Hook stderr is untrusted and may contain credentials or input
            // data. The bounded bytes remain available to the channel
            // contract, but audit logs record only the closed outcome.
            tracing::warn!("hooks: hook exited {exit_code:?} (non-blocking)");
            PreDecisionPart {
                verdict: Verdict::Defer,
                reason: None,
                updated_input: None,
            }
        }
        ChannelResult::TimedOut => {
            tracing::warn!("hooks: hook timed out");
            PreDecisionPart {
                verdict: Verdict::Defer,
                reason: None,
                updated_input: None,
            }
        }
        ChannelResult::OutputLimitExceeded => {
            tracing::warn!("hooks: hook exceeded its output limit");
            PreDecisionPart {
                verdict: Verdict::Defer,
                reason: None,
                updated_input: None,
            }
        }
        ChannelResult::PolicyDenied { reason } => PreDecisionPart {
            verdict: Verdict::Deny,
            reason: Some(format!("hook subprocess policy denied launch: {reason}")),
            updated_input: None,
        },
    }
}

/// Deterministic merge for `PreToolUse`: strict most-severe verdict wins;
/// `updatedInput` folds in declared order (later declarations overwrite
/// earlier ones), warning if more than one hook rewrote the input.
///
/// `Verdict`'s declared (and derived `Ord`) order is `Allow < Defer < Ask <
/// Deny` (least to most severe), so a lone `Allow` part is *less* than the
/// `Defer` "no opinion" sentinel — comparing with a fixed `Defer` starting
/// point would silently drop it. Seed from the first part actually seen
/// instead, so an all-`Allow` (or any single-part) result reflects the real
/// verdict rather than always regressing to `Defer`.
fn merge_pre_decisions(
    original_input: &serde_json::Value,
    parts: &[PreDecisionPart],
) -> PreDecision {
    let mut verdict = Verdict::Defer;
    let mut reason = None;
    let mut current_input = original_input.clone();
    let mut rewrite_count = 0;
    let mut seen_any = false;
    for part in parts {
        if !seen_any || part.verdict > verdict {
            verdict = part.verdict;
            reason = part.reason.clone();
        }
        seen_any = true;
        if let Some(rewrite) = &part.updated_input {
            current_input = rewrite.clone();
            rewrite_count += 1;
        }
    }
    if rewrite_count > 1 {
        tracing::warn!(
            "hooks: {rewrite_count} hooks rewrote tool input for the same call; using the last declared rewrite"
        );
    }
    PreDecision {
        verdict,
        reason,
        updated_input: (rewrite_count > 0).then_some(current_input),
    }
}

fn parse_decision(event: &str, output: &HookOutput) -> Decision {
    match interpret_hook_output(output) {
        ChannelResult::Block { stderr } => Decision::Block { reason: stderr },
        ChannelResult::NoObjection { json: Some(value) } => {
            if value.get("decision").and_then(|v| v.as_str()) == Some("block") {
                let reason = value
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                return Decision::Block { reason };
            }
            if event == "SubagentStart"
                && let Some(content) = value.get("additionalContext").and_then(|v| v.as_str())
            {
                return Decision::Rewrite {
                    content: content.to_string(),
                };
            }
            if value.get("additionalContext").is_some() || value.get("result").is_some() {
                tracing::warn!(
                    event = event,
                    "hooks: ignored unsafe hook-authored model context or result replacement"
                );
            }
            Decision::Continue
        }
        ChannelResult::NoObjection { json: None } => Decision::Continue,
        ChannelResult::Error { .. }
        | ChannelResult::TimedOut
        | ChannelResult::OutputLimitExceeded => Decision::Continue,
        ChannelResult::PolicyDenied { reason } => Decision::Block {
            reason: format!("hook subprocess policy denied launch: {reason}"),
        },
    }
}

/// Merges multiple hooks' generic decisions: any `Block` wins outright; else
/// the first event-permitted `Rewrite` wins; else `Continue`.
fn merge_decisions(event: &str, outputs: &[HookOutput]) -> Decision {
    let mut rewrite = None;
    for output in outputs {
        match parse_decision(event, output) {
            Decision::Block { reason } => return Decision::Block { reason },
            Decision::Rewrite { content } if rewrite.is_none() => {
                rewrite = Some(Decision::Rewrite { content });
            }
            _ => {}
        }
    }
    rewrite.unwrap_or(Decision::Continue)
}

fn merge_post_tool_use_decisions(outputs: &[HookOutput], tool_response: &str) -> Decision {
    let mut redacted = tool_response.to_string();
    let mut changed = false;

    for output in outputs {
        let ChannelResult::NoObjection { json: Some(value) } = interpret_hook_output(output) else {
            continue;
        };
        if value.get("result").is_some() || value.get("additionalContext").is_some() {
            tracing::warn!(
                event = "PostToolUse",
                "hooks: ignored unsafe hook-authored model context or result replacement"
            );
        }
        changed |= apply_redactions(&mut redacted, &value);
    }

    if changed {
        Decision::Rewrite { content: redacted }
    } else {
        Decision::Continue
    }
}

fn apply_redactions(tool_response: &mut String, value: &serde_json::Value) -> bool {
    let Some(redactions) = value.get("redactions").and_then(|value| value.as_array()) else {
        return false;
    };
    let mut changed = false;
    for literal in redactions
        .iter()
        .filter_map(|value| value.as_str())
        .filter(|literal| !literal.is_empty())
    {
        if tool_response.contains(literal) {
            *tool_response = tool_response.replace(literal, REDACTION_MARKER);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::apply_redactions;

    #[test]
    fn redactions_replace_only_requested_non_empty_literals() {
        let mut response = "token=secret; status=ok".to_string();

        let changed = apply_redactions(
            &mut response,
            &serde_json::json!({"redactions": ["secret", "", 42]}),
        );

        assert!(changed);
        assert_eq!(response, "token=[REDACTED]; status=ok");
    }

    #[test]
    fn missing_redactions_leave_the_response_unchanged() {
        let mut response = "authoritative result".to_string();

        let changed = apply_redactions(
            &mut response,
            &serde_json::json!({"result": "injected content"}),
        );

        assert!(!changed);
        assert_eq!(response, "authoritative result");
    }
}
