use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use regex::Regex;

use super::channel::{ChannelResult, interpret_hook_output};
use super::envelope::{EventFields, build_envelope};
use super::normalize::canonical_tool_name;
use super::settings::{HookHandler, HooksConfig};
use super::subprocess::{
    HookOutput, HookPolicy, HookStatus, run_hook_with_policy, run_shell_condition,
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

/// The rig-free dispatcher seam: accepts only strings/JSON and returns only
/// zerostack-owned `Decision`/`PreDecision` values. See hook-dispatch spec's
/// "rig-free dispatcher seam" requirement.
pub(crate) struct HookDispatcher {
    events: HashMap<String, Vec<MatcherEntry>>,
    sandbox_backend: String,
    /// Canonical startup project root. Production dispatch binds both the
    /// hook envelope and subprocess policy to this immutable directory.
    project_root: Option<String>,
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
            Some(project_root.to_string_lossy().into_owned()),
        )
    }

    fn from_config_with_backend_and_optional_root(
        config: &HooksConfig,
        sandbox_backend: &str,
        project_root: Option<String>,
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
        Ok(Self {
            events,
            sandbox_backend: sandbox_backend.to_string(),
            project_root,
            once_ran: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    fn policy_context(&self, ctx: &HookCtx) -> HookCtx {
        let mut ctx = ctx.clone();
        if let Some(project_root) = &self.project_root {
            ctx.cwd.clone_from(project_root);
        }
        ctx
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
        let ctx = self.policy_context(ctx);
        let envelope = build_envelope(&ctx, event, fields);
        let outputs = self
            .run_handlers(event, &handlers, &envelope, &ctx.cwd)
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
        let ctx = self.policy_context(ctx);
        let envelope = build_envelope(
            &ctx,
            "PreToolUse",
            EventFields::PreToolUse {
                tool_name: canonical,
                tool_input: tool_input.clone(),
            },
        );
        let outputs = self
            .run_handlers("PreToolUse", &handlers, &envelope, &ctx.cwd)
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
        let ctx = self.policy_context(ctx);
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
            .run_handlers("PostToolUse", &handlers, &envelope, &ctx.cwd)
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
        let ctx = self.policy_context(ctx);
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
            .run_handlers("PostToolUseFailure", &handlers, &envelope, &ctx.cwd)
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
    ) -> Vec<HookOutput> {
        let stdin = serde_json::to_vec(envelope).unwrap_or_default();
        let mut futures = Vec::new();
        let mut async_futures = Vec::new();
        for handler in handlers {
            let Some(command) = handler.command.clone() else {
                continue;
            };

            if let Some(condition) = &handler.condition {
                let policy =
                    HookPolicy::new(handler.trust, &self.sandbox_backend, handler.env.clone());
                let cond_timeout =
                    std::time::Duration::from_secs(handler.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));
                let cond_output =
                    run_shell_condition(condition, &stdin, cond_timeout, project_dir, &policy)
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
            if handler.is_async {
                async_futures.push(async move {
                    let output = run_hook_with_policy(
                        &command,
                        args.as_deref(),
                        &stdin,
                        timeout,
                        &project_dir,
                        &policy,
                    )
                    .await;
                    once_reservation.consume_if_started(&output);
                    audit_hook_outcome(&audit_event, &command, "handler", trust, &output);
                });
            } else {
                futures.push(async move {
                    let output = run_hook_with_policy(
                        &command,
                        args.as_deref(),
                        &stdin,
                        timeout,
                        &project_dir,
                        &policy,
                    )
                    .await;
                    once_reservation.consume_if_started(&output);
                    audit_hook_outcome(&audit_event, &command, "handler", trust, &output);
                    output
                });
            }
        }
        let (outputs, _) = tokio::join!(
            futures::future::join_all(futures),
            futures::future::join_all(async_futures)
        );
        outputs
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
