use std::collections::{HashMap, HashSet};

use regex::Regex;

use super::channel::{ChannelResult, interpret_hook_output};
use super::envelope::{EventFields, build_envelope};
use super::normalize::canonical_tool_name;
use super::settings::{HookHandler, HooksConfig};
use super::subprocess::{HookOutput, run_hook};
use super::{Decision, HookCtx, PreDecision, Verdict};

/// Default per-hook timeout when a handler doesn't declare one.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

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
    /// Handlers with `once: true` that have already run, keyed by
    /// `(event, command)` so the same command declared under two different
    /// events tracks independently.
    once_ran: std::sync::Mutex<HashSet<(String, String)>>,
}

impl HookDispatcher {
    pub(crate) fn from_config(config: &HooksConfig) -> Result<Self, String> {
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
            once_ran: std::sync::Mutex::new(HashSet::new()),
        })
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

    /// Handlers matching `event`/`canonical_tool_name`, in declared order,
    /// with identical commands deduplicated (first occurrence wins).
    pub(crate) fn handlers_for(&self, event: &str, canonical_tool_name: &str) -> Vec<&HookHandler> {
        let Some(entries) = self.events.get(event) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for entry in entries {
            if !entry.matcher.matches(canonical_tool_name) {
                continue;
            }
            for handler in &entry.handlers {
                if let Some(cmd) = handler.command.as_deref()
                    && !seen.insert(cmd)
                {
                    continue;
                }
                result.push(handler);
            }
        }
        result
    }

    /// Generic dispatch for non-permission events: lifecycle events and
    /// `PostToolUse` result rewrite. Returns `Decision::Continue` after only
    /// an index lookup when nothing matches (the zero-cost invariant).
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
        let envelope = build_envelope(ctx, event, fields);
        let outputs = self
            .run_handlers(event, &handlers, &envelope, &ctx.cwd)
            .await;
        merge_decisions(&outputs)
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
        let envelope = build_envelope(
            ctx,
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

    /// Dispatches `PostToolUse`: may rewrite the model-visible result.
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
        let envelope = build_envelope(
            ctx,
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
        merge_decisions(&outputs)
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
        let envelope = build_envelope(
            ctx,
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
        for handler in handlers {
            let Some(command) = handler.command.clone() else {
                continue;
            };

            if handler.once {
                let mut ran = self.once_ran.lock().unwrap_or_else(|e| e.into_inner());
                if !ran.insert((event.to_string(), command.clone())) {
                    continue;
                }
            }

            if let Some(condition) = &handler.condition {
                let cond_timeout =
                    std::time::Duration::from_secs(handler.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));
                let cond_output =
                    run_hook(condition, None, &stdin, cond_timeout, project_dir).await;
                if cond_output.timed_out {
                    tracing::warn!(
                        "hooks: `if` condition for {command:?} timed out; failing closed (running the handler)"
                    );
                } else {
                    match cond_output.exit_code {
                        Some(0) => {}
                        Some(_) => continue,
                        None => {
                            tracing::warn!(
                                "hooks: `if` condition for {command:?} could not be spawned; failing closed (running the handler)"
                            );
                        }
                    }
                }
            }

            let timeout =
                std::time::Duration::from_secs(handler.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));
            let stdin = stdin.clone();
            let project_dir = project_dir.to_string();
            let args = handler.args.clone();
            if handler.is_async {
                tokio::spawn(async move {
                    let _ =
                        run_hook(&command, args.as_deref(), &stdin, timeout, &project_dir).await;
                });
            } else {
                futures.push(async move {
                    run_hook(&command, args.as_deref(), &stdin, timeout, &project_dir).await
                });
            }
        }
        futures::future::join_all(futures).await
    }
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
        ChannelResult::Error { exit_code, stderr } => {
            tracing::warn!("hooks: hook exited {exit_code:?} (non-blocking): {stderr}");
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

fn parse_decision(output: &HookOutput) -> Decision {
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
            if let Some(content) = value.get("additionalContext").and_then(|v| v.as_str()) {
                return Decision::Rewrite {
                    content: content.to_string(),
                };
            }
            if let Some(content) = value.get("result").and_then(|v| v.as_str()) {
                return Decision::Rewrite {
                    content: content.to_string(),
                };
            }
            Decision::Continue
        }
        ChannelResult::NoObjection { json: None } => Decision::Continue,
        ChannelResult::Error { .. } | ChannelResult::TimedOut => Decision::Continue,
    }
}

/// Merges multiple hooks' generic decisions: any `Block` wins outright; else
/// the first declared `Rewrite` wins; else `Continue`.
fn merge_decisions(outputs: &[HookOutput]) -> Decision {
    let mut rewrite = None;
    for output in outputs {
        match parse_decision(output) {
            Decision::Block { reason } => return Decision::Block { reason },
            Decision::Rewrite { content } if rewrite.is_none() => {
                rewrite = Some(Decision::Rewrite { content });
            }
            _ => {}
        }
    }
    rewrite.unwrap_or(Decision::Continue)
}
