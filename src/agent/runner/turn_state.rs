//! The decisions of one agent turn, shared by the interactive, headless and
//! subagent loops (mini-agent-sskdu.101).
//!
//! The three loops differ in where their output goes (an `AgentEvent`
//! channel, a `HeadlessOutputPolicy`, a subagent log) and in how they stop
//! (an event, a failed `HeadlessTurn`, an `Err` result). They used to differ
//! in the turn logic too, because each carried its own copy of it. Everything
//! that decides something now lives here as a synchronous method over
//! [`TurnState`] returning a typed value; a loop only performs the I/O the
//! value calls for. There is no `await` in this module, so a loop's future
//! keeps its size and the loops' stack-budget tests are unaffected.

use rig::completion::{Message, Usage};
use rig::message::{ToolCall, ToolResult};

use super::{
    CompletionRetryState, CompletionVerification, DEFAULT_CONTINUATION, NonTerminalStreamExhausted,
    TRANSIENT_PROVIDER_CONTINUATION, ToolCallTracker, ToolCallTrackerError, UsageLedger,
    append_attributed_tool_result, append_streamed_text, append_tool_call,
    empty_response_limit_reached, exhausted_token_budget, is_max_turns_stream_error,
    midstream_retry, note_budget_after_completion, reconcile_final_output,
    reconcile_terminal_usage, take_accepted_goal_report, take_new_interactions,
    token_budget_stop_message, tool_may_mutate_workspace, verification_retry_instruction,
};
use crate::agent::output_truncation::{OutputTruncation, TruncationDecision};
use crate::event::UsageDelta;
use crate::retry::RetryConfig;

/// What ends a turn at a stream boundary (before another stream is started).
pub(super) enum TurnStop {
    /// The provider stream ended without a terminal response and the attempt
    /// budget is spent.
    NonTerminalEof(NonTerminalStreamExhausted),
    /// No model call is left.
    TurnBudget { max_turns: usize },
    /// The cumulative token budget is spent.
    TokenBudget(String),
}

impl TurnStop {
    pub(super) fn message(&self) -> String {
        match self {
            Self::NonTerminalEof(error) => error.to_string(),
            Self::TurnBudget { max_turns } => format!(
                "Agent exhausted its maximum turn budget ({max_turns}) before completing.{}",
                crate::cli::MAX_AGENT_TURNS_HINT
            ),
            Self::TokenBudget(message) => message.clone(),
        }
    }

    /// The error a headless turn fails with. The typed non-terminal error is
    /// kept so a caller can still downcast it.
    pub(super) fn into_error(self) -> anyhow::Error {
        match self {
            Self::NonTerminalEof(error) => anyhow::Error::new(error),
            other => anyhow::anyhow!(other.message()),
        }
    }
}

/// How a stream error is handled, once the loop has settled its own tool
/// calls and parked questions.
pub(super) enum StreamErrorAction {
    /// Rig reports every allowed model call was used.
    MaxTurns,
    /// The provider cut the output off at its token limit. `instruction` is
    /// `Some` when the turn continues (already stored as the next
    /// instruction) and `None` when the continuation budget is spent.
    Truncated {
        notice: String,
        instruction: Option<String>,
    },
    /// A transient error: wait `delay`, then resume from the transcript
    /// (the continuation instruction is already stored).
    Retry {
        attempt: usize,
        delay: std::time::Duration,
    },
    /// Not recoverable.
    Fatal,
}

/// What a finished completion gate means for the turn.
pub(super) enum VerificationNext {
    Passed,
    /// Failed with attempts left; the retry instruction is stored.
    Retry,
    /// Failed on the last attempt.
    Exhausted,
}

/// What a final response without text means.
pub(super) enum EmptyFinal {
    /// The limit of consecutive empty responses was reached.
    Abort,
    /// Ask the model to continue.
    Continue,
}

/// A stream boundary that starts another stream.
pub(super) struct Continuation {
    /// Model calls still available.
    pub(super) remaining_turns: usize,
    /// The instruction that continues the turn.
    pub(super) instruction: String,
    /// What the finished stream appended to the transcript.
    pub(super) new_interactions: Vec<Message>,
}

pub(super) struct TurnState {
    /// The transcript of the stream being consumed.
    pub(super) interactions: Vec<Message>,
    pub(super) tool_calls: ToolCallTracker,
    pub(super) output_truncation: OutputTruncation,
    pub(super) completion_retry: CompletionRetryState,
    pub(super) usage_ledger: UsageLedger,
    pub(super) turns_used: usize,
    pub(super) turns_at_stream_start: usize,
    /// Set when a completion crossed the token budget; later tool items fail.
    exhausted_budget_after_completion: Option<(u64, u64)>,
    turn_token_budget: Option<u64>,
    /// An accepted `goal_report` ended the round (mini-agent-64qyf).
    pub(super) goal_report_accepted: bool,
    pub(super) workspace_may_have_changed: bool,
    empty_response_count: u32,
    verification_attempt: u32,
    /// Overrides the default continuation message.
    pub(super) next_instruction: Option<String>,
    /// The response text of the current completion.
    pub(super) response: String,
    pub(super) response_len_at_stream_start: usize,
    #[cfg(feature = "hooks")]
    pub(super) stop_hook_active: bool,
    #[cfg(feature = "hooks")]
    consecutive_stop_blocks: u32,
}

impl TurnState {
    pub(super) fn new(retry_config: &RetryConfig, turn_token_budget: Option<u64>) -> Self {
        let mut usage_ledger = UsageLedger::default();
        usage_ledger.start_stream();
        Self {
            interactions: Vec::new(),
            tool_calls: ToolCallTracker::default(),
            output_truncation: OutputTruncation::default(),
            completion_retry: CompletionRetryState::new(retry_config),
            usage_ledger,
            turns_used: 0,
            turns_at_stream_start: 0,
            exhausted_budget_after_completion: None,
            turn_token_budget,
            goal_report_accepted: false,
            workspace_may_have_changed: false,
            empty_response_count: 0,
            verification_attempt: 0,
            next_instruction: None,
            response: String::new(),
            response_len_at_stream_start: 0,
            #[cfg(feature = "hooks")]
            stop_hook_active: false,
            #[cfg(feature = "hooks")]
            consecutive_stop_blocks: 0,
        }
    }

    /// The failure for a tool item after a completion crossed the token
    /// budget: no later tool item may start or deliver work.
    /// The token budget left for this turn, after what earlier segments of the
    /// same prompt already spent.
    pub(super) fn set_turn_token_budget(&mut self, budget: Option<u64>) {
        self.turn_token_budget = budget;
    }

    pub(super) fn budget_stop(&self) -> Option<String> {
        token_budget_stop_message(self.exhausted_budget_after_completion)
    }

    /// Streamed assistant text.
    pub(super) fn on_text(&mut self, text: &str) {
        self.response.push_str(text);
        append_streamed_text(&mut self.interactions, text);
    }

    /// A tool call the model issued. The turn's response is the text of its
    /// final completion, so text written before a call is its own transcript
    /// segment, not a prefix of the answer (mini-agent-knwk1).
    pub(super) fn on_tool_call(
        &mut self,
        internal_call_id: &str,
        tool_call: &ToolCall,
    ) -> Result<(), ToolCallTrackerError> {
        self.workspace_may_have_changed |= tool_may_mutate_workspace(&tool_call.function.name);
        self.tool_calls.record(internal_call_id, tool_call)?;
        self.response.clear();
        self.response_len_at_stream_start = 0;
        append_tool_call(&mut self.interactions, tool_call);
        Ok(())
    }

    /// A tool result: correlate it with its call, append it to the transcript
    /// and note an accepted goal report. `None` when no pending call matches.
    pub(super) fn on_tool_result(
        &mut self,
        internal_call_id: &str,
        tool_result: &ToolResult,
    ) -> Option<(compact_str::CompactString, String)> {
        let attributed = append_attributed_tool_result(
            &mut self.tool_calls,
            &mut self.interactions,
            internal_call_id,
            tool_result,
        )?;
        self.goal_report_accepted |= take_accepted_goal_report(internal_call_id);
        Some(attributed)
    }

    /// Bookkeeping common to every observed `CompletionCall`: output
    /// truncation, the mid-stream retry budget and the turn charge.
    pub(super) fn begin_completion(
        &mut self,
        call: &rig::agent::CompletionCall,
        retry_config: &RetryConfig,
    ) {
        self.output_truncation.observe_call(call);
        self.completion_retry.reset(retry_config);
        self.turns_used = self.turns_used.saturating_add(1);
    }

    /// A completion: [`Self::begin_completion`], then its usage folded into the
    /// ledger and the spend run. Returns the usage delta.
    pub(super) fn on_completion_call(
        &mut self,
        call: &rig::agent::CompletionCall,
        retry_config: &RetryConfig,
    ) -> UsageDelta {
        self.begin_completion(call, retry_config);
        let delta = self.usage_ledger.record(call.usage);
        crate::spend::run::observe_current(delta);
        delta
    }

    /// Remember whether the cumulative token budget is now spent. Called once
    /// the loop has finished reacting to the completion.
    pub(super) fn note_budget(&mut self) {
        note_budget_after_completion(
            self.usage_ledger.total,
            self.turn_token_budget,
            &mut self.exhausted_budget_after_completion,
        );
    }

    /// The terminal response's aggregate usage, as a correction delta.
    pub(super) fn reconcile_terminal(&mut self, usage: Usage) -> UsageDelta {
        reconcile_terminal_usage(&mut self.usage_ledger, usage)
    }

    /// Reconcile the terminal text with what was streamed; `true` when it had
    /// already been streamed.
    pub(super) fn reconcile_final(&mut self, terminal: &str) -> bool {
        reconcile_final_output(
            &mut self.response,
            self.response_len_at_stream_start,
            terminal,
            &mut self.interactions,
        )
    }

    /// The decision for a final response cut off at the output-token limit.
    /// A continuation stores its instruction.
    pub(super) fn final_truncation(&mut self) -> Option<(String, Option<String>)> {
        let decision = self.output_truncation.final_response()?;
        tracing::warn!("{}", decision.notice());
        let notice = decision.notice().to_string();
        Some(match decision {
            TruncationDecision::Continue { instruction, .. } => {
                self.next_instruction = Some(instruction.clone());
                (notice, Some(instruction))
            }
            TruncationDecision::Exhausted { .. } | TruncationDecision::Notice { .. } => {
                (notice, None)
            }
        })
    }

    /// A final response without text: abort after too many in a row, else ask
    /// the model to continue.
    pub(super) fn on_empty_final(&mut self) -> EmptyFinal {
        if empty_response_limit_reached(&mut self.empty_response_count) {
            tracing::warn!(
                "agent: {} consecutive empty responses, aborting",
                super::MAX_EMPTY_RESPONSES
            );
            EmptyFinal::Abort
        } else {
            EmptyFinal::Continue
        }
    }

    /// Whether the completion gate runs for this final response.
    pub(super) fn verification_applies(
        &self,
        verification: Option<&CompletionVerification>,
    ) -> bool {
        self.workspace_may_have_changed
            && verification.is_some_and(CompletionVerification::has_command)
    }

    /// Count a gate run; returns its one-based attempt number.
    pub(super) fn begin_verification(&mut self) -> u32 {
        self.verification_attempt = self.verification_attempt.saturating_add(1);
        self.verification_attempt
    }

    /// What a finished gate means. A retry stores its instruction.
    pub(super) fn verification_finished(
        &mut self,
        verification: &CompletionVerification,
        passed: bool,
        diagnostic: &str,
    ) -> VerificationNext {
        if passed {
            return VerificationNext::Passed;
        }
        if self.verification_attempt < verification.max_attempts {
            self.next_instruction = Some(verification_retry_instruction(
                verification.command.as_deref().unwrap_or(""),
                self.verification_attempt,
                verification.max_attempts,
                diagnostic,
            ));
            VerificationNext::Retry
        } else {
            VerificationNext::Exhausted
        }
    }

    /// A `Stop` hook blocked completion. `true` when the turn continues (the
    /// instruction is stored); `false` once the consecutive-block cap forces a
    /// release.
    #[cfg(feature = "hooks")]
    pub(super) fn stop_blocked(&mut self, reason: String) -> bool {
        match super::stop_block_continuation(
            &mut self.consecutive_stop_blocks,
            &mut self.stop_hook_active,
            reason,
        ) {
            Some(instruction) => {
                self.next_instruction = Some(instruction);
                true
            }
            None => false,
        }
    }

    /// How a stream error is handled. `may_retry` is `false` when the loop has
    /// no model call left to resume with.
    pub(super) fn on_stream_error(
        &mut self,
        error: &rig::agent::StreamingError,
        retry_config: &RetryConfig,
        may_retry: bool,
    ) -> StreamErrorAction {
        if is_max_turns_stream_error(error) {
            return StreamErrorAction::MaxTurns;
        }
        if let Some(decision) = self.output_truncation.stream_error(&error.to_string()) {
            tracing::warn!("{}: {error}", decision.notice());
            let notice = decision.notice().to_string();
            let instruction = match decision {
                TruncationDecision::Continue { instruction, .. } => {
                    self.next_instruction = Some(instruction.clone());
                    Some(instruction)
                }
                TruncationDecision::Exhausted { .. } | TruncationDecision::Notice { .. } => None,
            };
            return StreamErrorAction::Truncated {
                notice,
                instruction,
            };
        }
        if may_retry
            && let Some((attempt, delay)) =
                midstream_retry(&mut self.completion_retry, retry_config, error)
        {
            self.next_instruction = Some(TRANSIENT_PROVIDER_CONTINUATION.to_string());
            return StreamErrorAction::Retry { attempt, delay };
        }
        StreamErrorAction::Fatal
    }

    /// The boundary between two streams: charge a stream that ended without a
    /// terminal response, check the turn and token budgets, and take the
    /// transcript the finished stream produced. `Err` ends the turn.
    pub(super) fn begin_continuation(
        &mut self,
        terminal_response_seen: bool,
        retrying_interrupted_completion: bool,
        max_turns: usize,
    ) -> Result<Continuation, TurnStop> {
        if !terminal_response_seen && !retrying_interrupted_completion {
            super::charge_nonterminal_eof(
                &mut self.turns_used,
                self.turns_at_stream_start,
                max_turns,
            )
            .map_err(|error| {
                tracing::warn!(
                    attempts = error.attempts,
                    limit = error.limit,
                    "agent stream EOF budget exhausted"
                );
                TurnStop::NonTerminalEof(error)
            })?;
        }
        let remaining_turns = max_turns.saturating_sub(self.turns_used);
        if remaining_turns == 0 {
            tracing::warn!(
                "agent: maximum turn budget ({max_turns}) exhausted before continuation"
            );
            return Err(TurnStop::TurnBudget { max_turns });
        }
        if let Some(message) = token_budget_stop_message(exhausted_token_budget(
            self.usage_ledger.total,
            self.turn_token_budget,
        )) {
            tracing::warn!("agent: cumulative token budget exhausted before continuation");
            return Err(TurnStop::TokenBudget(message));
        }
        let instruction = self
            .next_instruction
            .take()
            .unwrap_or_else(|| DEFAULT_CONTINUATION.to_string());
        let new_interactions = take_new_interactions(&mut self.interactions);
        Ok(Continuation {
            remaining_turns,
            instruction,
            new_interactions,
        })
    }

    /// A new stream has been started after [`Self::begin_continuation`].
    pub(super) fn stream_started(&mut self) {
        self.usage_ledger.start_stream();
        self.turns_at_stream_start = self.turns_used;
        self.response_len_at_stream_start = self.response.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(token_budget: Option<u64>) -> TurnState {
        TurnState::new(&RetryConfig::default(), token_budget)
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall::new(
            id.to_string(),
            rig::message::ToolFunction {
                name: name.to_string(),
                arguments: serde_json::json!({}),
            },
        )
    }

    #[test]
    fn a_tool_call_starts_the_response_over_and_is_tracked() {
        let mut turn = state(None);
        turn.on_text("working on it");
        turn.on_tool_call("internal-1", &call("p1", "write"))
            .unwrap();
        assert!(turn.response.is_empty());
        assert_eq!(turn.response_len_at_stream_start, 0);
        assert!(turn.workspace_may_have_changed);
        assert_eq!(turn.tool_calls.pending.len(), 1);
        assert!(
            turn.on_tool_call("internal-1", &call("p1", "write"))
                .is_err(),
            "a duplicate internal id is refused"
        );
    }

    #[test]
    fn a_continuation_reports_what_ended_the_turn() {
        // Turn budget.
        let mut turn = state(None);
        turn.turns_used = 3;
        let stop = turn.begin_continuation(true, false, 3).err().unwrap();
        assert!(matches!(stop, TurnStop::TurnBudget { max_turns: 3 }));
        assert!(stop.message().contains("maximum turn budget (3)"));
        // A stream that ended without a terminal response costs a turn.
        let mut turn = state(None);
        turn.turns_used = 1;
        turn.turns_at_stream_start = 1;
        let stop = turn.begin_continuation(false, false, 2).err().unwrap();
        assert!(matches!(stop, TurnStop::NonTerminalEof(_)));
        assert!(stop.into_error().is::<NonTerminalStreamExhausted>());
        // Token budget.
        let mut turn = state(Some(10));
        turn.usage_ledger.record(Usage {
            input_tokens: 8,
            output_tokens: 8,
            total_tokens: 16,
            ..Usage::new()
        });
        let stop = turn.begin_continuation(true, false, 5).err().unwrap();
        assert!(matches!(stop, TurnStop::TokenBudget(_)));
    }

    #[test]
    fn a_continuation_uses_the_stored_instruction_once() {
        let mut turn = state(None);
        turn.next_instruction = Some("custom".to_string());
        let first = turn.begin_continuation(true, false, 5).ok().unwrap();
        assert_eq!(first.instruction, "custom");
        assert_eq!(first.remaining_turns, 5);
        let second = turn.begin_continuation(true, false, 5).ok().unwrap();
        assert_eq!(second.instruction, DEFAULT_CONTINUATION);
        turn.on_text("abc");
        turn.stream_started();
        assert_eq!(turn.response_len_at_stream_start, 3);
    }

    #[test]
    fn repeated_empty_final_responses_abort() {
        let mut turn = state(None);
        assert!(matches!(turn.on_empty_final(), EmptyFinal::Continue));
        assert!(matches!(turn.on_empty_final(), EmptyFinal::Continue));
        assert!(matches!(turn.on_empty_final(), EmptyFinal::Abort));
    }

    #[test]
    fn a_crossed_token_budget_stops_later_tool_items() {
        let mut turn = state(Some(10));
        assert!(turn.budget_stop().is_none());
        turn.usage_ledger.record(Usage {
            input_tokens: 6,
            output_tokens: 6,
            total_tokens: 12,
            ..Usage::new()
        });
        turn.note_budget();
        assert!(turn.budget_stop().unwrap().contains("token budget"));
    }
}
