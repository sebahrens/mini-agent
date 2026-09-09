use compact_str::CompactString;

/// A chargeable, provider-reported usage increment.
///
/// Runners reconcile a terminal aggregate against usage already observed for
/// the stream before constructing this value. Consumers can therefore add each
/// delta exactly once; terminal events never carry a second copy of usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageDelta {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub tool_use_prompt_tokens: u64,
    pub reasoning_tokens: u64,
}

impl UsageDelta {
    pub fn has_values(self) -> bool {
        self != Self::default()
    }
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
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
    #[cfg(any(feature = "subagents", feature = "acp"))]
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
    /// A terminal failure with no completed work to retain.
    #[cfg(feature = "hooks")]
    pub fn error(message: impl Into<CompactString>) -> Self {
        Self::Error {
            message: message.into(),
            interactions: Vec::new(),
        }
    }

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

#[cfg(feature = "loop")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidationOperationId(pub(crate) u64);

#[cfg(feature = "loop")]
#[derive(Debug, Clone)]
pub(crate) struct LoopValidationEvent {
    pub operation_id: ValidationOperationId,
    pub response: CompactString,
    pub summary: String,
    pub result: crate::extras::r#loop::validation::ValidationResult,
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    Key(crossterm::event::KeyEvent),
    ScrollUp,
    ScrollDown,
    Resize,
    Paste(String),
    #[cfg(feature = "loop")]
    LoopValidationDone(LoopValidationEvent),
    MouseDown {
        row: u16,
        col: u16,
    },
    MouseDrag {
        row: u16,
    },
    MouseUp {
        row: u16,
    },
    /// An interactive MCP OAuth login finished in a background task. `error` is
    /// `None` on success. Handled by the TUI loop to reconnect the server.
    #[cfg(feature = "mcp")]
    McpLoginDone {
        server: CompactString,
        error: Option<CompactString>,
    },
}
