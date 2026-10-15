use compact_str::CompactString;

/// A chargeable, provider-reported usage increment.
///
/// Runners reconcile a terminal aggregate against usage already observed for
/// the stream before constructing this value. Consumers can therefore add each
/// delta exactly once; terminal events never carry a second copy of usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageDelta {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    /// Reported write tiers, never inferred from a requested TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<rig::completion::CacheCreationUsage>,
    pub tool_use_prompt_tokens: u64,
    pub reasoning_tokens: u64,
}

impl UsageDelta {
    pub(crate) fn nonnegative_difference(self, previous: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            total_tokens: self.total_tokens.saturating_sub(previous.total_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(previous.cached_input_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_sub(previous.cache_creation_input_tokens),
            cache_creation: rig::completion::CacheCreationUsage::difference_optional(
                self.cache_creation_input_tokens,
                self.cache_creation,
                previous.cache_creation_input_tokens,
                previous.cache_creation,
            ),
            tool_use_prompt_tokens: self
                .tool_use_prompt_tokens
                .saturating_sub(previous.tool_use_prompt_tokens),
            reasoning_tokens: self
                .reasoning_tokens
                .saturating_sub(previous.reasoning_tokens),
        }
    }

    pub fn has_values(self) -> bool {
        [
            self.input_tokens,
            self.output_tokens,
            self.total_tokens,
            self.cached_input_tokens,
            self.cache_creation_input_tokens,
            self.tool_use_prompt_tokens,
            self.reasoning_tokens,
        ]
        .into_iter()
        .any(|count| count != 0)
    }

    /// Field-wise saturating sum, for totalling several reports of separate
    /// requests (never a stream's increments against its own aggregate).
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_add(other.cached_input_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(other.cache_creation_input_tokens),
            cache_creation: rig::completion::CacheCreationUsage::add_optional(
                self.cache_creation_input_tokens,
                self.cache_creation,
                other.cache_creation_input_tokens,
                other.cache_creation,
            ),
            tool_use_prompt_tokens: self
                .tool_use_prompt_tokens
                .saturating_add(other.tool_use_prompt_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    SteerConsumed {
        messages: Vec<rig::completion::Message>,
        checkpoint: Option<crate::session::file_checkpoints::SteeringHandoff>,
    },
    #[cfg_attr(not(feature = "hooks"), allow(dead_code))]
    SteerRejected {
        reason: String,
    },
    Token(CompactString),
    Reasoning(CompactString),
    ToolCall {
        id: CompactString,
        name: CompactString,
        args: serde_json::Value,
    },
    ToolResult {
        id: CompactString,
        name: CompactString,
        output: CompactString,
        /// Validated literal image content, bound to this exact correlated result.
        images: Vec<crate::session::tool_image::PersistedToolImage>,
        /// The tool failed, was denied, or its outcome is unknown. Clients that
        /// report tool status (ACP) mark such calls failed.
        #[cfg_attr(not(feature = "acp"), allow(dead_code))]
        is_error: bool,
    },
    /// Display-only, throttled preview of a running foreground tool call's
    /// output (currently `shell`), bound to the call's internal id. It is
    /// never persisted or shown to the model: the call's `ToolResult` stays
    /// the only record. Clients without a live view ignore it.
    ToolProgress {
        id: CompactString,
        text: CompactString,
    },
    /// Native successful mutation metadata, bound by the SDK hook to the
    /// exact internal call. Never reconstructed from model-visible text.
    FileMutation {
        id: CompactString,
        name: CompactString,
        diff: crate::agent::tools::mutation_diff::MutationDiff,
    },
    /// The runner has honored a UI compaction request only after every
    /// in-flight tool result is correlated into this structured transcript.
    CompactionBoundary {
        interactions: Vec<rig::message::Message>,
    },
    /// A run-scoped guard replaced or skipped a repetitive tool call. The
    /// ordinary `ToolResult` is still emitted and persisted; this event gives
    /// interactive clients an explicit diagnostic without parsing it.
    ToolLoop {
        name: CompactString,
        message: CompactString,
    },
    #[cfg(feature = "subagents")]
    SubagentToolCall {
        name: CompactString,
        args: serde_json::Value,
    },
    #[cfg(feature = "subagents")]
    SubagentStarted {
        agent_type: CompactString,
        source: CompactString,
    },
    /// A terminal failure, with the canonical messages for work that already
    /// completed in this turn. A tool effect can happen and a later provider
    /// failure end the turn; a client that commits history only on `Done`
    /// would otherwise have no record of the effect.
    Error {
        message: CompactString,
        interactions: Vec<rig::completion::Message>,
    },
    /// Completed progress retained while the exact main turn waits for a new
    /// ordinary user message. This is data, never an approval or live reply.
    AwaitingUser {
        question: Box<crate::question::QuestionRequired>,
        interactions: Vec<rig::completion::Message>,
    },
    Retrying {
        attempt: usize,
        max: usize,
    },
    Verification {
        attempt: u32,
        max: u32,
        passed: bool,
        output: CompactString,
    },
    /// The sole chargeable usage stream. A delta usually represents one
    /// provider completion call; it can also be a terminal reconciliation when
    /// an adapter reports only an aggregate.
    UsageDelta {
        usage: UsageDelta,
        /// True when this delta is also a complete single-call usage snapshot
        /// suitable for context pressure/calibration. A partial terminal
        /// reconciliation remains chargeable but must not replace the last
        /// complete context observation.
        context_complete: bool,
        /// True only when the interactive runner is parked before continuing
        /// this provider/tool loop and requires a UI boundary decision.
        awaits_compaction_decision: bool,
    },
    Done {
        response: CompactString,
        /// Canonical Rig messages produced during this completed turn. Tool
        /// call/result IDs come from the provider, so downstream history users
        /// never need to reconstruct model interactions from display events.
        interactions: Vec<rig::completion::Message>,
    },
}

impl AgentEvent {
    /// A terminal failure that happened after `interactions` already completed.
    pub fn error_with(
        message: impl Into<CompactString>,
        interactions: Vec<rig::completion::Message>,
    ) -> Self {
        Self::Error {
            message: message.into(),
            interactions,
        }
    }
}

/// Events emitted by an isolated `/btw` side-question run. Kept as a separate
/// type from [`AgentEvent`] so that a side-question result can never be routed
/// through `handle_agent_event` (which mutates the session): the type system
/// enforces that `/btw` leaves no trace in conversation history.
#[derive(Debug, Clone)]
pub enum BtwEvent {
    Done {
        id: u32,
        response: CompactString,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        cache_creation_input_tokens: u64,
    },
    Error {
        id: u32,
        message: CompactString,
    },
}

#[cfg(any(feature = "loop", feature = "goal"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidationOperationId(pub(crate) u64);

/// A goal round's verification finished off the event loop.
///
/// Checks and a judge call can each take many seconds. Running them inside the
/// turn handler would hold the event loop and make the interface unresponsive,
/// including to an interrupt, so they run on their own task and come back as
/// an event.
#[cfg(feature = "goal")]
#[derive(Debug, Clone)]
pub(crate) struct GoalVerificationEvent {
    pub operation_id: ValidationOperationId,
    pub checks: Option<crate::extras::goal::gate::CheckOutcome>,
    pub judge: Option<crate::extras::goal::gate::JudgeOutcome>,
    /// Tokens the judge call spent, for the goal's token total and budget.
    pub judge_tokens: u64,
    /// The judge call's usage priced at the judge model's own rates, charged
    /// to the session whether or not the result still applies.
    pub judge_charge: Option<crate::extras::goal::judge::JudgeCharge>,
    /// The operator stopped the check while it was running. Nothing was proved
    /// either way, so the round settles as interrupted rather than judged on a
    /// command that was cancelled.
    pub interrupted: bool,
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    #[cfg(feature = "mcp")]
    McpAdminDone {
        id: u64,
    },
    Key(crossterm::event::KeyEvent),
    ScrollUp,
    ScrollDown,
    Resize,
    Paste(String),
    LinkOpenFailed(String),
    /// Boxed: this variant carries a whole verification result, and
    /// `UserEvent` is held in the interactive future the startup path builds on
    /// the stack. A regression test runs the binary under the 1 MiB Windows
    /// default stack.
    #[cfg(feature = "goal")]
    GoalVerificationDone(Box<GoalVerificationEvent>),
    MouseDown {
        row: u16,
        col: u16,
    },
    MouseDrag {
        row: u16,
        col: u16,
    },
    MouseUp {
        row: u16,
        col: u16,
    },
    /// Leave the TUI in an orderly way (termination signal, lost terminal):
    /// stop any run, save the session and restore the terminal.
    Quit,
}
