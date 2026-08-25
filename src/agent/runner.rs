use compact_str::CompactString;
use futures::StreamExt;
use rig::OneOrMany;
use rig::agent::{Agent, MultiTurnStreamItem, StreamingResult};
use rig::completion::Usage;
#[cfg(feature = "multimodal")]
use rig::completion::message::{AudioMediaType, DocumentMediaType, ImageMediaType};
use rig::completion::{CompletionModel, Message};
use rig::message::{AssistantContent, ToolCall, ToolResult, ToolResultContent};
use rig::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingChat};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;
use tokio::sync::mpsc;

use crate::event::{AgentEvent, BtwEvent, UsageDelta};
#[cfg(feature = "hooks")]
use crate::extras::hooks::LoopInfo;
use crate::retry::{self, RetryConfig};
use crate::session::{MessageRole, Session};

pub struct AgentRunner {
    pub event_rx: mpsc::Receiver<AgentEvent>,
    /// Cancels the underlying agent task. Without this a superseded or
    /// interrupted run keeps driving its stream — and therefore keeps executing
    /// tools (edit/write/bash) — invisibly. Aborting stops it for real.
    pub abort_handle: tokio::task::AbortHandle,
}

tokio::task_local! {
    static AGENT_WORK_SCOPE: Arc<AgentWorkScope>;
}

/// Tracks child work which can outlive the future that started it (notably
/// `spawn_blocking`). ACP cancellation signals the scope, drops provider/tool
/// polling, and waits for every registered child before closing the event
/// channel and returning `Cancelled`.
pub(crate) struct AgentWorkScope {
    cancelled: AtomicBool,
    active_children: AtomicUsize,
    cancellation: Notify,
    idle: Notify,
    #[cfg(test)]
    blocking_gate: Option<Arc<BlockingTestGate>>,
}

impl AgentWorkScope {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
            active_children: AtomicUsize::new(0),
            cancellation: Notify::new(),
            idle: Notify::new(),
            #[cfg(test)]
            blocking_gate: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_with_blocking_test_gate() -> (
        Arc<Self>,
        std::sync::mpsc::Receiver<()>,
        BlockingTestRelease,
    ) {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let released = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let gate = Arc::new(BlockingTestGate {
            started_tx,
            released: Arc::clone(&released),
        });
        (
            Arc::new(Self {
                cancelled: AtomicBool::new(false),
                active_children: AtomicUsize::new(0),
                cancellation: Notify::new(),
                idle: Notify::new(),
                blocking_gate: Some(gate),
            }),
            started_rx,
            BlockingTestRelease(released),
        )
    }

    pub(crate) async fn run<F: std::future::Future>(self: &Arc<Self>, future: F) -> F::Output {
        AGENT_WORK_SCOPE.scope(Arc::clone(self), future).await
    }

    pub(crate) fn cancellation_handle(self: &Arc<Self>) -> AgentWorkCancellation {
        AgentWorkCancellation {
            scope: Arc::clone(self),
        }
    }

    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            self.cancellation.notify_waiters();
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn active_children(&self) -> usize {
        self.active_children.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.cancellation.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    pub(crate) async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.active_children.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    fn register(self: &Arc<Self>) -> AgentWorkGuard {
        self.active_children.fetch_add(1, Ordering::AcqRel);
        AgentWorkGuard {
            scope: Arc::clone(self),
        }
    }
}

#[derive(Clone)]
pub(crate) struct AgentWorkCancellation {
    scope: Arc<AgentWorkScope>,
}

impl AgentWorkCancellation {
    pub(crate) fn cancel(&self) {
        self.scope.cancel();
    }

    #[cfg(test)]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.scope.is_cancelled()
    }
}

pub(crate) struct AgentWorkGuard {
    scope: Arc<AgentWorkScope>,
}

/// Keeps the runner event channel open through hard task abortion. Dropping
/// the outer Tokio future signals cancellation synchronously, then transfers
/// scope settlement and the lifecycle sender to an independent waiter.
pub(crate) struct AgentRunCleanupGuard {
    scope: Arc<AgentWorkScope>,
    lifecycle_tx: Option<mpsc::Sender<AgentEvent>>,
    runtime: tokio::runtime::Handle,
}

impl AgentRunCleanupGuard {
    pub(crate) fn new(scope: Arc<AgentWorkScope>, lifecycle_tx: mpsc::Sender<AgentEvent>) -> Self {
        Self {
            scope,
            lifecycle_tx: Some(lifecycle_tx),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub(crate) async fn settle(mut self) {
        self.scope.cancellation_handle().cancel();
        self.scope.wait_idle().await;
        self.lifecycle_tx.take();
    }
}

impl Drop for AgentRunCleanupGuard {
    fn drop(&mut self) {
        let Some(lifecycle_tx) = self.lifecycle_tx.take() else {
            return;
        };
        self.scope.cancellation_handle().cancel();
        let scope = Arc::clone(&self.scope);
        std::mem::drop(self.runtime.spawn(async move {
            scope.wait_idle().await;
            drop(lifecycle_tx);
        }));
    }
}

#[cfg(test)]
struct BlockingTestGate {
    started_tx: std::sync::mpsc::Sender<()>,
    released: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

#[cfg(test)]
pub(crate) struct BlockingTestRelease(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);

#[cfg(test)]
impl BlockingTestRelease {
    pub(crate) fn release(&self) {
        let (released, wake) = &*self.0;
        *released.lock().unwrap_or_else(|error| error.into_inner()) = true;
        wake.notify_all();
    }
}

impl Drop for AgentWorkGuard {
    fn drop(&mut self) {
        if self.scope.active_children.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.scope.idle.notify_waiters();
        }
    }
}

pub(crate) fn spawn_blocking_scoped<F, R>(operation: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    spawn_blocking_scoped_on(&tokio::runtime::Handle::current(), operation)
}

/// Spawns asynchronous lifecycle work owned by the current agent turn. Outside
/// an agent scope this preserves ordinary Tokio spawn semantics.
pub(crate) fn spawn_async_scoped<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    if let Ok(scope) = AGENT_WORK_SCOPE.try_with(Arc::clone) {
        let guard = scope.register();
        tokio::spawn(async move {
            let _guard = guard;
            scope.run(future).await
        })
    } else {
        tokio::spawn(future)
    }
}

#[cfg(feature = "skills")]
pub(crate) fn current_work_guard() -> Option<AgentWorkGuard> {
    AGENT_WORK_SCOPE.try_with(|scope| scope.register()).ok()
}

pub(crate) fn spawn_blocking_scoped_on<F, R>(
    runtime: &tokio::runtime::Handle,
    operation: F,
) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let scoped = AGENT_WORK_SCOPE
        .try_with(|scope| {
            (
                scope.register(),
                #[cfg(test)]
                scope.blocking_gate.clone(),
            )
        })
        .ok();
    runtime.spawn_blocking(move || {
        #[cfg(test)]
        if let Some((_, Some(gate))) = &scoped {
            let _ = gate.started_tx.send(());
            let (released, wake) = &*gate.released;
            let mut released = released.lock().unwrap_or_else(|error| error.into_inner());
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }
        let _guard = scoped;
        operation()
    })
}

#[cfg(any(feature = "hooks", test))]
pub(crate) async fn current_work_scope_cancelled() {
    let scope = AGENT_WORK_SCOPE.try_with(Arc::clone).ok();
    match scope {
        Some(scope) => scope.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

pub(crate) struct PausedAgentRunner {
    runner: AgentRunner,
    start_tx: Option<tokio::sync::oneshot::Sender<()>>,
    work_scope: Arc<AgentWorkScope>,
}

impl PausedAgentRunner {
    pub(crate) fn new(
        runner: AgentRunner,
        start_tx: tokio::sync::oneshot::Sender<()>,
        work_scope: Arc<AgentWorkScope>,
    ) -> Self {
        Self {
            runner,
            start_tx: Some(start_tx),
            work_scope,
        }
    }

    /// The caller publishes this handle before releasing the start barrier.
    pub(crate) fn cancellation_handle(&self) -> AgentWorkCancellation {
        self.work_scope.cancellation_handle()
    }

    pub(crate) fn start(mut self) -> AgentRunner {
        if let Some(start_tx) = self.start_tx.take() {
            let _ = start_tx.send(());
        }
        self.runner
    }
}

/// Handle to an in-flight `/btw` side-question task. The `abort_handle` lets the
/// UI cancel the side question (e.g. on Ctrl-C) without touching the main agent.
pub struct BtwRunner {
    pub abort_handle: tokio::task::AbortHandle,
    pub task: tokio::task::JoinHandle<()>,
    pub(crate) work_scope: Arc<AgentWorkScope>,
}

fn streamed_reasoning_text<R>(content: &StreamedAssistantContent<R>) -> Option<CompactString> {
    match content {
        StreamedAssistantContent::Reasoning(reasoning) => {
            Some(CompactString::new(reasoning.display_text()))
        }
        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
            if reasoning.is_empty() {
                None
            } else {
                Some(CompactString::from(reasoning.as_str()))
            }
        }
        _ => None,
    }
}

fn warn_unknown_stream_item<R: serde::Serialize>(item: &MultiTurnStreamItem<R>) {
    let detail = serde_json::to_string(item)
        .unwrap_or_else(|error| format!("<failed to serialize stream item: {error}>"));
    tracing::warn!("unknown stream item variant: {detail}");
}

const MAX_PENDING_TOOL_CALLS: usize = 256;
const UNKNOWN_TOOL_OUTCOME: &str =
    "[Tool execution outcome is unknown because the agent stream ended before a result.]";
const UNRESOLVED_TOOL_CALLS_ERROR: &str =
    "Agent stream ended with unresolved tool calls; execution outcome is unknown.";

#[derive(Debug)]
struct PendingToolCall {
    provider_id: String,
    provider_call_id: Option<String>,
    name: String,
}

#[derive(Default)]
struct ToolCallTracker {
    pending: HashMap<String, PendingToolCall>,
    order: VecDeque<String>,
}

#[derive(Debug)]
struct FinalizedToolCall {
    internal_call_id: String,
    name: String,
    #[cfg(test)]
    tool_result: ToolResult,
}

impl ToolCallTracker {
    fn record(
        &mut self,
        internal_call_id: &str,
        tool_call: &ToolCall,
    ) -> Result<(), ToolCallTrackerError> {
        if internal_call_id.is_empty() {
            tracing::error!(
                tool_name = %tool_call.function.name,
                provider_tool_call_id = %tool_call.id,
                "agent received tool call without an internal correlation ID"
            );
            return Err(ToolCallTrackerError::MissingInternalId);
        }
        if self.pending.contains_key(internal_call_id) {
            tracing::error!(
                internal_call_id,
                provider_tool_call_id = %tool_call.id,
                "agent received duplicate pending internal tool call ID"
            );
            return Err(ToolCallTrackerError::DuplicateInternalId);
        }
        if self.pending.len() >= MAX_PENDING_TOOL_CALLS {
            tracing::error!(
                limit = MAX_PENDING_TOOL_CALLS,
                internal_call_id,
                provider_tool_call_id = %tool_call.id,
                "agent pending tool-call correlation limit reached"
            );
            return Err(ToolCallTrackerError::CapacityExceeded);
        }
        self.pending.insert(
            internal_call_id.to_owned(),
            PendingToolCall {
                provider_id: tool_call.id.clone(),
                provider_call_id: tool_call.call_id.clone(),
                name: tool_call.function.name.clone(),
            },
        );
        self.order.push_back(internal_call_id.to_owned());
        Ok(())
    }

    fn take(
        &mut self,
        internal_call_id: &str,
        tool_result: &ToolResult,
    ) -> Option<PendingToolCall> {
        if internal_call_id.is_empty() {
            tracing::error!(
                provider_tool_result_id = %tool_result.id,
                "agent received tool result without an internal correlation ID; skipping"
            );
            return None;
        }
        let Some(call) = self.pending.get(internal_call_id) else {
            tracing::error!(
                internal_call_id,
                provider_tool_result_id = %tool_result.id,
                "agent received unknown or duplicate internal tool result ID; skipping"
            );
            return None;
        };
        if call.provider_id != tool_result.id || call.provider_call_id != tool_result.call_id {
            tracing::error!(
                internal_call_id,
                provider_tool_call_id = %call.provider_id,
                provider_tool_result_id = %tool_result.id,
                provider_tool_call_call_id = ?call.provider_call_id,
                provider_tool_result_call_id = ?tool_result.call_id,
                "provider tool correlation IDs differ for an internally correlated call/result pair; preserving the pending call"
            );
            return None;
        }
        let call = self.pending.remove(internal_call_id)?;
        if let Some(position) = self.order.iter().position(|id| id == internal_call_id) {
            self.order.remove(position);
        } else {
            tracing::error!(
                internal_call_id,
                "tool-call correlation order was missing a pending ID"
            );
        }
        Some(call)
    }

    fn finalize_unresolved(&mut self, interactions: &mut Vec<Message>) -> Vec<FinalizedToolCall> {
        let mut finalized = Vec::with_capacity(self.pending.len());
        while let Some(internal_call_id) = self.order.pop_front() {
            let Some(call) = self.pending.remove(&internal_call_id) else {
                continue;
            };
            let tool_result = ToolResult {
                id: call.provider_id,
                call_id: call.provider_call_id,
                content: OneOrMany::one(ToolResultContent::text(UNKNOWN_TOOL_OUTCOME)),
            };
            interactions.push(tool_result.clone().into());
            finalized.push(FinalizedToolCall {
                internal_call_id,
                name: call.name,
                #[cfg(test)]
                tool_result,
            });
        }
        if !finalized.is_empty() {
            tracing::warn!(
                pending_tool_calls = finalized.len(),
                "agent stream ended with unresolved tool calls; appended explicit unknown-outcome results"
            );
        }
        finalized
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum ToolCallTrackerError {
    #[error("Agent received a tool call without an internal correlation ID.")]
    MissingInternalId,
    #[error("Agent received a duplicate pending internal tool call ID.")]
    DuplicateInternalId,
    #[error("Agent exceeded the pending tool-call correlation limit ({MAX_PENDING_TOOL_CALLS}).")]
    CapacityExceeded,
}

fn attributed_tool_result(
    tracker: &mut ToolCallTracker,
    internal_call_id: &str,
    tool_result: &ToolResult,
) -> Option<(CompactString, String)> {
    let call = tracker.take(internal_call_id, tool_result)?;

    let mut output = String::new();
    let mut text_content_count = 0usize;
    let mut non_text_content_count = 0usize;
    for content in tool_result.content.iter() {
        match content {
            ToolResultContent::Text(text) => {
                text_content_count += 1;
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&text.text);
            }
            ToolResultContent::Image(_) => non_text_content_count += 1,
        }
    }

    if text_content_count == 0 {
        tracing::warn!(
            internal_call_id,
            provider_tool_result_id = %tool_result.id,
            non_text_content_count,
            "agent tool result contained no text content; using a visible fallback"
        );
        output
            .push_str("[Tool result contained non-text content that cannot be displayed as text.]");
    }

    Some((CompactString::new(call.name), output))
}

fn append_attributed_tool_result(
    tracker: &mut ToolCallTracker,
    interactions: &mut Vec<Message>,
    internal_call_id: &str,
    tool_result: &ToolResult,
) -> Option<(CompactString, String)> {
    let attributed = attributed_tool_result(tracker, internal_call_id, tool_result)?;
    interactions.push(tool_result.clone().into());
    Some(attributed)
}

async fn finalize_interactive_tool_calls(
    tracker: &mut ToolCallTracker,
    interactions: &mut Vec<Message>,
    event_tx: &mpsc::Sender<AgentEvent>,
) -> usize {
    let finalized = tracker.finalize_unresolved(interactions);
    let count = finalized.len();
    for call in finalized {
        let _ = event_tx
            .send(AgentEvent::ToolResult {
                id: CompactString::from(call.internal_call_id),
                name: CompactString::from(call.name),
                output: CompactString::from(UNKNOWN_TOOL_OUTCOME),
            })
            .await;
    }
    count
}

impl From<Usage> for UsageDelta {
    fn from(usage: Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            tool_use_prompt_tokens: usage.tool_use_prompt_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

impl From<UsageDelta> for Usage {
    fn from(delta: UsageDelta) -> Self {
        Self {
            input_tokens: delta.input_tokens,
            output_tokens: delta.output_tokens,
            total_tokens: delta.total_tokens,
            cached_input_tokens: delta.cached_input_tokens,
            cache_creation_input_tokens: delta.cache_creation_input_tokens,
            tool_use_prompt_tokens: delta.tool_use_prompt_tokens,
            reasoning_tokens: delta.reasoning_tokens,
        }
    }
}

#[derive(Default)]
struct UsageLedger {
    total: Usage,
    stream_observed: Usage,
}

impl UsageLedger {
    fn record(&mut self, usage: Usage) -> UsageDelta {
        self.total = usage_saturating_add(self.total, usage);
        self.stream_observed = usage_saturating_add(self.stream_observed, usage);
        usage.into()
    }

    fn start_stream(&mut self) {
        self.stream_observed = Usage::new();
    }

    fn reconcile_terminal(&mut self, aggregate: Usage) -> UsageDelta {
        let delta = usage_nonnegative_difference(aggregate, self.stream_observed, true);
        self.total = usage_saturating_add(self.total, delta);
        delta.into()
    }

    fn stream_has_observed_usage(&self) -> bool {
        self.stream_observed.has_values()
    }
}

#[cfg(feature = "subagents")]
#[derive(Clone, Default)]
pub(crate) struct SharedUsageLedger {
    inner: std::sync::Arc<std::sync::Mutex<UsageLedger>>,
}

#[cfg(feature = "subagents")]
impl SharedUsageLedger {
    pub(crate) fn record_completion(&self, usage: Usage) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(usage);
    }

    fn reconcile_terminal(&self, usage: Usage) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reconcile_terminal(usage);
    }

    pub(crate) fn total(&self) -> Usage {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .total
    }
}

pub(crate) fn usage_saturating_add(left: Usage, right: Usage) -> Usage {
    Usage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        total_tokens: left.total_tokens.saturating_add(right.total_tokens),
        cached_input_tokens: left
            .cached_input_tokens
            .saturating_add(right.cached_input_tokens),
        cache_creation_input_tokens: left
            .cache_creation_input_tokens
            .saturating_add(right.cache_creation_input_tokens),
        tool_use_prompt_tokens: left
            .tool_use_prompt_tokens
            .saturating_add(right.tool_use_prompt_tokens),
        reasoning_tokens: left.reasoning_tokens.saturating_add(right.reasoning_tokens),
    }
}

fn usage_nonnegative_difference(
    aggregate: Usage,
    observed: Usage,
    warn_on_regression: bool,
) -> Usage {
    macro_rules! difference {
        ($field:ident) => {{
            if warn_on_regression && aggregate.$field < observed.$field {
                tracing::warn!(
                    field = stringify!($field),
                    aggregate = aggregate.$field,
                    observed = observed.$field,
                    "provider terminal usage regressed below observed deltas; charging no terminal delta for this field"
                );
            }
            aggregate.$field.saturating_sub(observed.$field)
        }};
    }
    Usage {
        input_tokens: difference!(input_tokens),
        output_tokens: difference!(output_tokens),
        total_tokens: difference!(total_tokens),
        cached_input_tokens: difference!(cached_input_tokens),
        cache_creation_input_tokens: difference!(cache_creation_input_tokens),
        tool_use_prompt_tokens: difference!(tool_use_prompt_tokens),
        reasoning_tokens: difference!(reasoning_tokens),
    }
}

fn completed_stream_delta(
    messages: Option<Vec<Message>>,
    current_prompt: &str,
) -> Result<Vec<Message>, &'static str> {
    let mut messages = messages.ok_or("Rig final response omitted completed messages")?;
    let expected_prompt = Message::user(current_prompt.to_owned());
    if messages.first() != Some(&expected_prompt) {
        return Err("Rig final response did not begin with the current prompt");
    }
    messages.remove(0);
    Ok(messages)
}

fn observed_tokens(usage: Usage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .max(usage.total_tokens)
}

fn exhausted_token_budget(usage: Usage, budget: Option<u64>) -> Option<(u64, u64)> {
    let budget = budget?;
    let used = observed_tokens(usage);
    (used >= budget).then_some((used, budget))
}

fn token_budget_exhaustion_message(used: u64, budget: u64) -> String {
    format!(
        "Agent exhausted its cumulative token budget ({used}/{budget}) before completing. \
         Compact the session or increase max_tokens before retrying."
    )
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "Agent stream ended without a terminal response; provider attempt budget exhausted ({attempts}/{limit})."
)]
struct NonTerminalStreamExhausted {
    attempts: usize,
    limit: usize,
}

/// Charge a provider stream that reached EOF without a terminal response.
///
/// Rig normally emits one `CompletionCall` for each provider call, including
/// calls whose raw stream ends without a provider final marker. Some provider
/// adapters can end before that accounting event, though, so an EOF must cost
/// at least one turn. The `turns_at_stream_start` snapshot lets this compose
/// with normal completion-call accounting without double-charging it.
fn charge_nonterminal_eof(
    turns_used: &mut usize,
    turns_at_stream_start: usize,
    max_turns: usize,
) -> Result<(), NonTerminalStreamExhausted> {
    if *turns_used == turns_at_stream_start {
        *turns_used = turns_used.saturating_add(1);
    }

    if *turns_used >= max_turns {
        return Err(NonTerminalStreamExhausted {
            attempts: *turns_used,
            limit: max_turns,
        });
    }

    Ok(())
}

fn append_streamed_text(interactions: &mut Vec<Message>, text: &str) {
    if text.is_empty() {
        return;
    }

    if let Some(Message::Assistant { content, .. }) = interactions.last_mut() {
        if let AssistantContent::Text(previous) = content.last_mut()
            && previous.additional_params.is_none()
        {
            previous.text.push_str(text);
        } else {
            content.push(AssistantContent::text(text));
        }
        return;
    }

    interactions.push(Message::assistant(text.to_string()));
}

fn append_tool_call(interactions: &mut Vec<Message>, tool_call: &ToolCall) {
    if let Some(Message::Assistant { content, .. }) = interactions.last_mut() {
        content.push(AssistantContent::ToolCall(tool_call.clone()));
    } else {
        interactions.push(tool_call.clone().into());
    }
}

fn reconcile_terminal_response(response: &mut String, stream_start: usize, terminal: &str) {
    if response.len() > stream_start {
        return;
    }

    response.push_str(terminal);
}

#[derive(Clone, Default)]
struct RunnerStreamPolicy {
    #[cfg(test)]
    drop_terminal_responses: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    drop_completion_calls: bool,
    #[cfg(test)]
    drop_tool_results: bool,
}

impl RunnerStreamPolicy {
    #[cfg(test)]
    fn drop_next_terminal_responses(count: usize) -> Self {
        Self {
            drop_terminal_responses: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                count,
            )),
            drop_completion_calls: false,
            drop_tool_results: false,
        }
    }

    #[cfg(test)]
    fn without_completion_calls() -> Self {
        Self {
            drop_completion_calls: true,
            ..Self::default()
        }
    }

    #[cfg(test)]
    fn without_tool_results() -> Self {
        Self {
            drop_tool_results: true,
            ..Self::default()
        }
    }

    #[cfg(test)]
    fn tool_call_text_eof() -> Self {
        Self {
            drop_terminal_responses: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            drop_tool_results: true,
            ..Self::default()
        }
    }

    fn apply<R>(&self, stream: StreamingResult<R>) -> StreamingResult<R>
    where
        R: Send + 'static,
    {
        #[cfg(test)]
        {
            let drop_terminal = self
                .drop_terminal_responses
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| (remaining > 0).then(|| remaining - 1),
                )
                .is_ok();
            let drop_completion_calls = self.drop_completion_calls;
            let drop_tool_results = self.drop_tool_results;
            if drop_terminal || drop_completion_calls || drop_tool_results {
                return stream
                    .filter(move |item| {
                        let is_terminal = matches!(item, Ok(MultiTurnStreamItem::FinalResponse(_)));
                        let is_completion =
                            matches!(item, Ok(MultiTurnStreamItem::CompletionCall(_)));
                        let is_tool_result = matches!(
                            item,
                            Ok(MultiTurnStreamItem::StreamUserItem(
                                StreamedUserContent::ToolResult { .. }
                            ))
                        );
                        std::future::ready(
                            !(drop_terminal && is_terminal
                                || drop_completion_calls && is_completion
                                || drop_tool_results && is_tool_result),
                        )
                    })
                    .boxed();
            }
        }

        stream
    }
}

/// Spawn an isolated, single-turn, tool-less side-question run. The full result
/// is delivered as a single [`BtwEvent::Done`] (or [`BtwEvent::Error`]) tagged
/// with `id`. Unlike [`spawn_agent`], it never registers a subagent event sink
/// and never mutates the session.
pub fn spawn_btw<M>(
    agent: Agent<M>,
    prompt: String,
    history: Vec<Message>,
    event_tx: mpsc::Sender<BtwEvent>,
    id: u32,
    retry_config: RetryConfig,
) -> BtwRunner
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let btw_future = async move {
        let stream_result = {
            let agent_ref = &agent;
            retry::retry_stream_chat(&retry_config, move || {
                let p = prompt.clone();
                let h = history.clone();
                async move { agent_ref.stream_chat(p, h).await }
            })
            .await
        };
        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                let _ = event_tx
                    .send(BtwEvent::Error {
                        id,
                        message: CompactString::new(e.to_string()),
                    })
                    .await;
                return;
            }
        };

        let mut acc = String::new();

        while let Some(item) = stream.next().await {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(
                    text,
                ))) => acc.push_str(&text.text),
                Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                    let usage = res.usage();
                    let response_text = res.output;
                    let response = if response_text.is_empty() {
                        CompactString::from(acc.as_str())
                    } else {
                        CompactString::from(response_text)
                    };
                    let _ = event_tx
                        .send(BtwEvent::Done {
                            id,
                            response,
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cached_input_tokens: usage.cached_input_tokens,
                            cache_creation_input_tokens: usage.cache_creation_input_tokens,
                        })
                        .await;
                    return;
                }
                Err(e) => {
                    let _ = event_tx
                        .send(BtwEvent::Error {
                            id,
                            message: CompactString::new(e.to_string()),
                        })
                        .await;
                    return;
                }
                _ => {}
            }
        }

        let _ = event_tx
            .send(BtwEvent::Error {
                id,
                message: CompactString::new("side question ended without a response"),
            })
            .await;
    };
    let work_scope = AgentWorkScope::new();
    let task_scope = Arc::clone(&work_scope);
    let join = tokio::spawn(async move {
        task_scope.run(btw_future).await;
    });

    BtwRunner {
        abort_handle: join.abort_handle(),
        task: join,
        work_scope,
    }
}

pub fn convert_history(session: &Session) -> Vec<Message> {
    let (summary, first_kept) = session.compacted_context();
    let remaining = session.messages.len().saturating_sub(first_kept);
    let extra = if summary.is_some() { 1 } else { 0 };
    let mut messages = Vec::with_capacity(remaining + extra);

    // The compaction summary is emitted as an Assistant message rather
    // than a System message: the agent already has a System preamble
    // (SYSTEM_PROMPT + mode prompt + context files), and some model chat
    // templates (notably Qwen 3.x) refuse any System message past
    // position 0. Assistant role also produces clean User↔Assistant
    // alternation when the next user prompt arrives, which reads as
    // "the agent recaps what it did, then the user continues" — a
    // natural resumed-conversation shape. The "[Recap of my prior work
    // in this conversation]" prefix labels the message as a self-recap
    // so the agent doesn't treat it as a fresh continuation of its own
    // voice.
    if let Some(summary) = summary {
        messages.push(Message::assistant(format!(
            "[Recap of my prior work in this conversation]\n{}",
            summary
        )));
    }

    for msg in &session.messages[first_kept..] {
        match msg.role {
            MessageRole::User => messages.push(Message::user(msg.content.to_string())),
            MessageRole::Assistant => messages.push(Message::assistant(msg.content.to_string())),
            // Convert non-user transcript records to Assistant for the
            // same reason as the summary above: the templates that reject
            // mid-stream System/tool roles tolerate Assistant, and code-symmetry with
            // the summary push keeps the resumed-conversation shape
            // consistent.
            MessageRole::System => messages.push(Message::assistant(msg.content.to_string())),
            MessageRole::ToolCall => {
                messages.push(Message::assistant(format!("[ToolCall]: {}", msg.content)))
            }
            MessageRole::ToolResult => {
                messages.push(Message::assistant(format!("[ToolResult]: {}", msg.content)))
            }
            MessageRole::SubagentToolCall => messages.push(Message::assistant(format!(
                "[SubagentToolCall]: {}",
                msg.content
            ))),
        }
    }

    messages
}

#[cfg(feature = "multimodal")]
pub fn media_to_messages(media: &[crate::extras::multimodal::MediaAttachment]) -> Vec<Message> {
    use rig::OneOrMany;
    use rig::completion::message::UserContent;

    media
        .iter()
        .map(|m| match m {
            crate::extras::multimodal::MediaAttachment::Image { data, mime, .. } => Message::User {
                content: OneOrMany::one(UserContent::image_raw(
                    data.clone(),
                    Some(image_media_type(mime)),
                    None,
                )),
            },
            crate::extras::multimodal::MediaAttachment::Audio { data, mime, .. } => Message::User {
                content: OneOrMany::one(UserContent::audio_raw(
                    data.clone(),
                    Some(audio_media_type(mime)),
                )),
            },
            crate::extras::multimodal::MediaAttachment::Document { data, mime, .. } => {
                Message::User {
                    content: OneOrMany::one(UserContent::document_raw(
                        data.clone(),
                        Some(document_media_type(mime)),
                    )),
                }
            }
        })
        .collect()
}

#[cfg(feature = "multimodal")]
fn image_media_type(mime: &str) -> ImageMediaType {
    match mime {
        "image/png" => ImageMediaType::PNG,
        "image/jpeg" => ImageMediaType::JPEG,
        "image/gif" => ImageMediaType::GIF,
        "image/webp" => ImageMediaType::WEBP,
        other => {
            tracing::warn!("unknown image mime type: {other}, defaulting to PNG");
            ImageMediaType::PNG
        }
    }
}

#[cfg(feature = "multimodal")]
fn audio_media_type(mime: &str) -> AudioMediaType {
    match mime {
        "audio/mpeg" => AudioMediaType::MP3,
        "audio/wav" => AudioMediaType::WAV,
        "audio/ogg" => AudioMediaType::OGG,
        "audio/flac" => AudioMediaType::FLAC,
        "audio/mp4" => AudioMediaType::M4A,
        "audio/aac" => AudioMediaType::AAC,
        other => {
            tracing::warn!("unknown audio mime type: {other}, defaulting to MP3");
            AudioMediaType::MP3
        }
    }
}

#[cfg(feature = "multimodal")]
fn document_media_type(mime: &str) -> DocumentMediaType {
    match mime {
        "application/pdf" => DocumentMediaType::PDF,
        other => {
            tracing::warn!("unknown document mime type: {other}, defaulting to PDF");
            DocumentMediaType::PDF
        }
    }
}

async fn continue_prompt_injector<M>(
    agent: &Agent<M>,
    original_prompt: &str,
    retry_history: &[Message],
    continuation_bridge: &[Message],
    retry_config: &RetryConfig,
    max_turns: usize,
) -> StreamingResult<M::StreamingResponse>
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let (current_prompt, bridge_history) = continuation_bridge
        .split_last()
        .expect("continuation bridge always ends with the continuation prompt");
    debug_assert!(matches!(current_prompt, Message::User { .. }));
    let mut new_history = retry_history.to_vec();
    new_history.push(Message::user(original_prompt.to_string()));
    new_history.extend_from_slice(bridge_history);
    match retry::retry_stream_chat(retry_config, || {
        let h = new_history.clone();
        let prompt = current_prompt.clone();
        async move { agent.stream_chat(prompt, h).max_turns(max_turns).await }
    })
    .await
    {
        Ok(stream) => stream,
        Err(e) => Box::pin(futures::stream::once(async move { Err(e) })),
    }
}

fn normalized_continuation_bridge(
    completed_interactions: &[Message],
    uncommitted_interactions: &[Message],
    continuation_instruction: &str,
) -> Vec<Message> {
    let mut bridge = completed_interactions.to_vec();
    bridge.extend_from_slice(uncommitted_interactions);
    if matches!(bridge.last(), Some(Message::User { .. })) {
        bridge.push(Message::assistant(String::new()));
    }
    bridge.push(Message::user(continuation_instruction.to_string()));
    bridge
}

fn take_new_interactions(interactions: &mut Vec<Message>) -> Vec<Message> {
    let new_interactions = std::mem::take(interactions);
    tracing::debug!(
        "agent injecting continue prompt, new_interactions={}",
        new_interactions.len(),
    );
    new_interactions
}

/// Builds the forked context for a `/btw` side question: the committed
/// conversation history, plus — when the main agent is mid-task — a synthesized
/// note describing the in-flight turn so the side question can see what the
/// agent is doing right now. The returned messages are a by-value snapshot; the
/// session is never mutated, so there is nothing to roll back afterwards.
pub fn build_btw_snapshot(
    session: &Session,
    turn_trace: &[CompactString],
    main_running: bool,
) -> Vec<Message> {
    let mut snapshot = convert_history(session);
    if main_running && !turn_trace.is_empty() {
        snapshot.push(Message::user(format!(
            "(Context only — the main assistant is working in parallel right now. \
Its progress so far this turn:\n{}\nThe last step may still be running. Use this \
only if the user's question is about what the main assistant is doing.)",
            turn_trace.join("\n")
        )));
    }
    snapshot
}

#[cfg(test)]
pub fn spawn_agent<M>(
    agent: Agent<M>,
    prompt: String,
    history: Vec<Message>,
    retry_config: RetryConfig,
    #[cfg(feature = "skills")] turn_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    // `--loop` iteration/active state, for the `Stop` hook envelope's
    // `loop_iteration`/`loop_active` fields (per-iteration reset of
    // `stop_hook_active`/the block cap falls out for free: each iteration is
    // a fresh call to this function). `None` outside loop mode.
    #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
) -> AgentRunner
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let work_scope = AgentWorkScope::new();
    spawn_agent_with_start_mode(
        agent,
        prompt,
        history,
        retry_config,
        RunnerStreamPolicy::default(),
        #[cfg(feature = "skills")]
        turn_guard,
        #[cfg(feature = "hooks")]
        loop_info,
        false,
        work_scope,
    )
    .0
}

#[cfg(test)]
pub(crate) fn spawn_agent_paused<M>(
    agent: Agent<M>,
    prompt: String,
    history: Vec<Message>,
    retry_config: RetryConfig,
    #[cfg(feature = "skills")] turn_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
) -> PausedAgentRunner
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    spawn_agent_paused_in_scope(
        agent,
        prompt,
        history,
        retry_config,
        #[cfg(feature = "skills")]
        turn_guard,
        #[cfg(feature = "hooks")]
        loop_info,
        AgentWorkScope::new(),
    )
}

pub(crate) fn spawn_agent_paused_in_scope<M>(
    agent: Agent<M>,
    prompt: String,
    history: Vec<Message>,
    retry_config: RetryConfig,
    #[cfg(feature = "skills")] turn_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
    work_scope: Arc<AgentWorkScope>,
) -> PausedAgentRunner
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let (runner, start_tx) = spawn_agent_with_start_mode(
        agent,
        prompt,
        history,
        retry_config,
        RunnerStreamPolicy::default(),
        #[cfg(feature = "skills")]
        turn_guard,
        #[cfg(feature = "hooks")]
        loop_info,
        true,
        Arc::clone(&work_scope),
    );
    PausedAgentRunner::new(
        runner,
        start_tx.expect("paused agent runner must have a start sender"),
        work_scope,
    )
}

#[cfg(test)]
fn spawn_agent_with_stream_policy<M>(
    agent: Agent<M>,
    prompt: String,
    history: Vec<Message>,
    retry_config: RetryConfig,
    stream_policy: RunnerStreamPolicy,
    #[cfg(feature = "skills")] turn_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
) -> AgentRunner
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let work_scope = AgentWorkScope::new();
    spawn_agent_with_start_mode(
        agent,
        prompt,
        history,
        retry_config,
        stream_policy,
        #[cfg(feature = "skills")]
        turn_guard,
        #[cfg(feature = "hooks")]
        loop_info,
        false,
        work_scope,
    )
    .0
}

#[allow(clippy::too_many_arguments)]
fn spawn_agent_with_start_mode<M>(
    agent: Agent<M>,
    prompt: String,
    history: Vec<Message>,
    retry_config: RetryConfig,
    stream_policy: RunnerStreamPolicy,
    #[cfg(feature = "skills")] turn_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
    start_paused: bool,
    work_scope: Arc<AgentWorkScope>,
) -> (AgentRunner, Option<tokio::sync::oneshot::Sender<()>>)
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(32);
    let runner_lifecycle_tx = event_tx.clone();
    let (start_tx, start_rx) = if start_paused {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    #[cfg(feature = "subagents")]
    let subagent_event_tx = event_tx.clone();

    let agent_future = async move {
        if let Some(start_rx) = start_rx
            && start_rx.await.is_err()
        {
            return;
        }
        #[cfg(feature = "skills")]
        let _turn_guard = turn_guard;
        tracing::debug!(
            "spawn_agent: prompt_len={}, history_len={}, max_attempts={}",
            prompt.len(),
            history.len(),
            retry_config.max_attempts,
        );
        // Keep the caller-owned inputs as the one canonical replay payload.
        // Rig requires an owned prompt/history for each API attempt, so the
        // attempt below clones exactly once; avoid making additional full
        // copies before any request has even started.
        let retry_prompt = prompt;
        let retry_history = history;
        let mut interactions: Vec<Message> = Vec::new();
        let mut tool_calls = ToolCallTracker::default();
        let mut completion_had_tool_call = false;
        let mut exhausted_budget_after_completion = None;
        let mut completed_interactions: Vec<Message> = Vec::new();
        // `None` means the initial prompt. Continuations install only their
        // small replacement instruction instead of cloning the initial prompt.
        let mut stream_prompt: Option<String> = None;
        let mut empty_response_count: u32 = 0;
        const MAX_EMPTY_RESPONSES: u32 = 3;
        let max_turns = agent.default_max_turns.unwrap_or(1);
        let mut turns_used = 0usize;
        let mut turns_at_stream_start = turns_used;
        let mut response = String::new();
        let mut response_len_at_stream_start = response.len();
        let mut usage_ledger = UsageLedger::default();
        usage_ledger.start_stream();
        // Overrides the next continuation message (bottom of the outer
        // `loop`); set when a `Stop` hook forces continuation instead of the
        // default re-injected `retry_prompt`.
        let mut next_instruction: Option<String> = None;
        #[cfg(feature = "hooks")]
        let mut stop_hook_active = false;
        #[cfg(feature = "hooks")]
        let mut consecutive_stop_blocks: u32 = 0;
        #[cfg(feature = "hooks")]
        const MAX_STOP_BLOCKS: u32 = 8;

        if max_turns == 0 {
            let _ = event_tx
                .send(AgentEvent::Error(CompactString::from(
                    "Agent exhausted its maximum turn budget (0) before starting.",
                )))
                .await;
            return;
        }

        let stream: StreamingResult<M::StreamingResponse> = {
            let mut attempt: usize = 0;
            let mut backoff = std::time::Duration::from_millis(retry_config.initial_backoff_ms);
            let max_backoff = std::time::Duration::from_millis(retry_config.max_backoff_ms);
            loop {
                attempt += 1;
                let mut s = agent
                    .stream_chat(retry_prompt.clone(), retry_history.clone())
                    .max_turns(max_turns)
                    .await;
                let first = s.next().await;
                match first {
                    Some(Ok(item)) => {
                        break futures::stream::once(std::future::ready(Ok(item)))
                            .chain(s)
                            .boxed();
                    }
                    Some(Err(e))
                        if attempt < retry_config.max_attempts && retry::is_retryable(&e) =>
                    {
                        tracing::warn!(
                            "agent retry {attempt}/{max} after error: {e}",
                            max = retry_config.max_attempts,
                        );
                        let _ = event_tx
                            .send(AgentEvent::Retrying {
                                attempt,
                                max: retry_config.max_attempts,
                            })
                            .await;
                        let jitter = retry::simple_jitter(backoff.as_millis() as u64);
                        tokio::time::sleep(backoff + jitter).await;
                        backoff = (backoff * 2).min(max_backoff);
                    }
                    Some(Err(e)) => {
                        tracing::error!("agent non-retryable error on attempt {attempt}: {e}");
                        let _ = event_tx
                            .send(AgentEvent::Error(CompactString::new(e.to_string())))
                            .await;
                        return;
                    }
                    None => break s.boxed(),
                }
            }
        };
        let mut stream = stream_policy.apply(stream);

        loop {
            let mut terminal_response_seen = false;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => {
                        if let Some(reasoning) = streamed_reasoning_text(&content) {
                            let _ = event_tx.send(AgentEvent::Reasoning(reasoning)).await;
                            continue;
                        }

                        match content {
                            StreamedAssistantContent::Text(text) => {
                                response.push_str(&text.text);
                                append_streamed_text(&mut interactions, &text.text);
                                let _ = event_tx
                                    .send(AgentEvent::Token(CompactString::from(text.text)))
                                    .await;
                            }
                            StreamedAssistantContent::ToolCall {
                                tool_call,
                                internal_call_id,
                            } => {
                                if let Some((used, budget)) = exhausted_budget_after_completion {
                                    finalize_interactive_tool_calls(
                                        &mut tool_calls,
                                        &mut interactions,
                                        &event_tx,
                                    )
                                    .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Error(CompactString::from(
                                            token_budget_exhaustion_message(used, budget),
                                        )))
                                        .await;
                                    return;
                                }
                                let tool_name = &tool_call.function.name;
                                tracing::debug!(
                                    "agent tool start: name={}, internal_call_id={}, args_len={}",
                                    tool_name,
                                    internal_call_id,
                                    tool_call.function.arguments.to_string().len(),
                                );
                                if let Err(error) = tool_calls.record(&internal_call_id, &tool_call)
                                {
                                    finalize_interactive_tool_calls(
                                        &mut tool_calls,
                                        &mut interactions,
                                        &event_tx,
                                    )
                                    .await;
                                    let _ = event_tx
                                        .send(AgentEvent::Error(CompactString::new(
                                            error.to_string(),
                                        )))
                                        .await;
                                    return;
                                }
                                completion_had_tool_call = true;
                                response.clear();
                                response_len_at_stream_start = 0;
                                append_tool_call(&mut interactions, &tool_call);
                                let _ = event_tx
                                    .send(AgentEvent::ToolCall {
                                        id: CompactString::from(internal_call_id),
                                        name: CompactString::from(tool_call.function.name),
                                        args: tool_call.function.arguments,
                                    })
                                    .await;
                            }
                            StreamedAssistantContent::Unknown(value) => {
                                warn_unknown_stream_item(&MultiTurnStreamItem::<
                                    M::StreamingResponse,
                                >::StreamAssistantItem(
                                    StreamedAssistantContent::Unknown(value),
                                ));
                            }
                            StreamedAssistantContent::ToolCallDelta { .. }
                            | StreamedAssistantContent::Reasoning(_)
                            | StreamedAssistantContent::ReasoningDelta { .. }
                            | StreamedAssistantContent::Final(_) => {}
                        }
                    }
                    Ok(MultiTurnStreamItem::ToolExecutionStart {
                        tool_call,
                        internal_call_id,
                    }) => {
                        if let Some((used, budget)) = exhausted_budget_after_completion {
                            finalize_interactive_tool_calls(
                                &mut tool_calls,
                                &mut interactions,
                                &event_tx,
                            )
                            .await;
                            let _ = event_tx
                                .send(AgentEvent::Error(CompactString::from(
                                    token_budget_exhaustion_message(used, budget),
                                )))
                                .await;
                            return;
                        }
                        tracing::debug!(
                            tool_name = %tool_call.function.name,
                            internal_call_id,
                            "agent tool execution started"
                        );
                    }
                    Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                        tool_result,
                        internal_call_id,
                    })) => {
                        if let Some((used, budget)) = exhausted_budget_after_completion {
                            finalize_interactive_tool_calls(
                                &mut tool_calls,
                                &mut interactions,
                                &event_tx,
                            )
                            .await;
                            let _ = event_tx
                                .send(AgentEvent::Error(CompactString::from(
                                    token_budget_exhaustion_message(used, budget),
                                )))
                                .await;
                            return;
                        }
                        let Some((tool_name, output)) = append_attributed_tool_result(
                            &mut tool_calls,
                            &mut interactions,
                            &internal_call_id,
                            &tool_result,
                        ) else {
                            continue;
                        };
                        tracing::debug!(
                            "agent tool result: name={}, output_len={}",
                            tool_name,
                            output.len(),
                        );
                        let _ = event_tx
                            .send(AgentEvent::ToolResult {
                                id: CompactString::from(internal_call_id),
                                name: tool_name.clone(),
                                output: CompactString::from(output),
                            })
                            .await;
                    }
                    Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                        terminal_response_seen = true;
                        let completed_delta = match completed_stream_delta(
                            res.messages.clone(),
                            stream_prompt.as_deref().unwrap_or(&retry_prompt),
                        ) {
                            Ok(messages) => messages,
                            Err(error) => {
                                tracing::error!(error, "agent final history invariant failed");
                                let _ = event_tx
                                    .send(AgentEvent::Error(CompactString::from(error)))
                                    .await;
                                return;
                            }
                        };
                        completed_interactions.extend(completed_delta);
                        let usage = res.usage();
                        let context_complete = !usage_ledger.stream_has_observed_usage();
                        let terminal_delta = usage_ledger.reconcile_terminal(usage);
                        if terminal_delta.has_values() {
                            let _ = event_tx
                                .send(AgentEvent::UsageDelta {
                                    usage: terminal_delta,
                                    context_complete,
                                })
                                .await;
                        }
                        let response_text = res.output;
                        let terminal_was_streamed = response.len() > response_len_at_stream_start;
                        reconcile_terminal_response(
                            &mut response,
                            response_len_at_stream_start,
                            &response_text,
                        );
                        if !terminal_was_streamed && !response_text.is_empty() {
                            append_streamed_text(&mut interactions, &response_text);
                        }
                        tracing::info!(
                            "agent done: input_tokens={}, output_tokens={}, cached_input_tokens={}, cache_creation_input_tokens={}",
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.cached_input_tokens,
                            usage.cache_creation_input_tokens,
                        );

                        if finalize_interactive_tool_calls(
                            &mut tool_calls,
                            &mut interactions,
                            &event_tx,
                        )
                        .await
                            > 0
                        {
                            let _ = event_tx
                                .send(AgentEvent::Error(CompactString::from(
                                    UNRESOLVED_TOOL_CALLS_ERROR,
                                )))
                                .await;
                            return;
                        }

                        if !response_text.is_empty() {
                            #[cfg(feature = "hooks")]
                            if let crate::extras::hooks::StopGate::Continue { reason } =
                                crate::extras::hooks::dispatch_stop(
                                    stop_hook_active,
                                    loop_info.map(|info| u64::from(info.iteration)),
                                    loop_info.map(|info| info.active),
                                )
                                .await
                            {
                                consecutive_stop_blocks += 1;
                                if consecutive_stop_blocks <= MAX_STOP_BLOCKS {
                                    stop_hook_active = true;
                                    tracing::info!(
                                        "hooks: Stop hook forced continuation ({consecutive_stop_blocks}/{MAX_STOP_BLOCKS}): {reason}"
                                    );
                                    next_instruction = Some(reason);
                                    break;
                                }
                                tracing::warn!(
                                    "hooks: Stop block cap ({MAX_STOP_BLOCKS}) reached without progress; forcing release"
                                );
                            }
                            let _ = event_tx
                                .send(AgentEvent::Done {
                                    response: CompactString::from(response.clone()),
                                    interactions: completed_interactions,
                                })
                                .await;
                            return;
                        }
                        empty_response_count += 1;
                        if empty_response_count >= MAX_EMPTY_RESPONSES {
                            tracing::warn!(
                                "agent: {MAX_EMPTY_RESPONSES} consecutive empty responses, aborting"
                            );
                            let _ = event_tx
                                .send(AgentEvent::Error(CompactString::from(
                                    "Agent returned empty response too many times, aborting.",
                                )))
                                .await;
                            return;
                        }
                        break;
                    }
                    Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                        turns_used = turns_used.saturating_add(1);
                        let usage = call.usage;
                        let delta = usage_ledger.record(usage);
                        tracing::debug!(
                            "agent completion: input_tokens={}, output_tokens={}, cumulative_tokens={}",
                            usage.input_tokens,
                            usage.output_tokens,
                            observed_tokens(usage_ledger.total),
                        );
                        if delta.has_values() {
                            let _ = event_tx
                                .send(AgentEvent::UsageDelta {
                                    usage: delta,
                                    context_complete: true,
                                })
                                .await;
                        }
                        if let Some((used, budget)) =
                            exhausted_token_budget(usage_ledger.total, agent.max_tokens)
                        {
                            if completion_had_tool_call {
                                tracing::warn!(
                                    used,
                                    budget,
                                    "agent cumulative token budget exhausted before the next provider call"
                                );
                                finalize_interactive_tool_calls(
                                    &mut tool_calls,
                                    &mut interactions,
                                    &event_tx,
                                )
                                .await;
                                let _ = event_tx
                                    .send(AgentEvent::Error(CompactString::from(
                                        token_budget_exhaustion_message(used, budget),
                                    )))
                                    .await;
                                return;
                            }
                            exhausted_budget_after_completion = Some((used, budget));
                        }
                        completion_had_tool_call = false;
                    }
                    Err(e) => {
                        tracing::error!("agent stream error: {e}");
                        finalize_interactive_tool_calls(
                            &mut tool_calls,
                            &mut interactions,
                            &event_tx,
                        )
                        .await;
                        let _ = event_tx
                            .send(AgentEvent::Error(CompactString::new(e.to_string())))
                            .await;
                        return;
                    }
                    Ok(item) => warn_unknown_stream_item(&item),
                }
            }

            finalize_interactive_tool_calls(&mut tool_calls, &mut interactions, &event_tx).await;

            if !terminal_response_seen
                && let Err(error) =
                    charge_nonterminal_eof(&mut turns_used, turns_at_stream_start, max_turns)
            {
                tracing::warn!(
                    attempts = error.attempts,
                    limit = error.limit,
                    "agent stream EOF budget exhausted"
                );
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::new(error.to_string())))
                    .await;
                return;
            }

            let remaining_turns = max_turns.saturating_sub(turns_used);
            if remaining_turns == 0 {
                tracing::warn!(
                    "agent: maximum turn budget ({max_turns}) exhausted before continuation"
                );
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::from(format!(
                        "Agent exhausted its maximum turn budget ({max_turns}) before completing."
                    ))))
                    .await;
                return;
            }
            if let Some((used, budget)) =
                exhausted_token_budget(usage_ledger.total, agent.max_tokens)
            {
                tracing::warn!(
                    "agent: cumulative token budget exhausted before continuation ({used}/{budget})"
                );
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::from(format!(
                        "Agent exhausted its cumulative token budget ({used}/{budget}) before \
                         completing. Compact the session or increase max_tokens before retrying."
                    ))))
                    .await;
                return;
            }
            let continuation_instruction = next_instruction
                .take()
                .unwrap_or_else(|| "Please continue.".to_string());
            let new_interactions = take_new_interactions(&mut interactions);
            // A non-terminal EOF has no Rig FinalResponse to supply an
            // authoritative delta. Preserve the replay payload only in that
            // exceptional path; completed streams use Rig's grouped messages.
            let uncommitted_interactions = if terminal_response_seen {
                &[][..]
            } else {
                new_interactions.as_slice()
            };
            let continuation_bridge = normalized_continuation_bridge(
                &completed_interactions,
                uncommitted_interactions,
                &continuation_instruction,
            );
            completed_interactions = continuation_bridge.clone();
            stream_prompt = Some(continuation_instruction.clone());
            stream = stream_policy.apply(
                continue_prompt_injector(
                    &agent,
                    &retry_prompt,
                    &retry_history,
                    &continuation_bridge,
                    &retry_config,
                    remaining_turns,
                )
                .await,
            );
            usage_ledger.start_stream();
            turns_at_stream_start = turns_used;
            response_len_at_stream_start = response.len();
        }
    };

    let task_scope = Arc::clone(&work_scope);
    // Construct before spawning so abort-before-first-poll still drops the
    // guard and transfers cleanup ownership.
    let cleanup = AgentRunCleanupGuard::new(Arc::clone(&task_scope), runner_lifecycle_tx);
    let agent_future = async move {
        let cancellation_scope = Arc::clone(&task_scope);
        task_scope
            .run(async move {
                tokio::select! {
                    biased;
                    _ = cancellation_scope.cancelled() => {}
                    _ = agent_future => {}
                }
            })
            .await;
        // A successful terminal event is also a settlement boundary. This
        // stops async:true hooks without waiting for their declared timeout.
        cleanup.settle().await;
    };

    #[cfg(feature = "subagents")]
    let agent_future =
        crate::extras::subagents::scope_subagent_event_tx(subagent_event_tx, agent_future);

    let join = tokio::spawn(agent_future);

    (
        AgentRunner {
            event_rx,
            abort_handle: join.abort_handle(),
        },
        start_tx,
    )
}

/// Headless (`-p`, `--loop`) counterpart to [`spawn_agent`]'s turn loop.
/// Deliberately drives its own manual loop instead of rig's
/// `.max_turns(max_turns)` combinator: `max_turns` is an opaque black box
/// that only ever yields a single terminal `FinalResponse` for the whole
/// session, with no seam to inject "one more turn" after it — exactly what a
/// `Stop` hook needs to do. Each stream is explicitly bounded to the unused
/// portion of the agent's `default_max_turns`, so hook continuations share one
/// model-call budget with the initial stream.
pub async fn run_print<M>(
    agent: &Agent<M>,
    prompt: &str,
    pure_stdout: bool,
    retry_config: &RetryConfig,
    // Prior turns from a resumed session (e.g. `--continue`), converted via
    // `convert_history`. Fed to the initial `stream_chat` call below and
    // seeded into `retry_history` for the hooks `Stop`-continuation retry,
    // mirroring `spawn_agent`. Empty for a fresh session.
    history: Vec<Message>,
    // `--loop` iteration/active state, for the `Stop` hook envelope's
    // `loop_iteration`/`loop_active` fields; see `runner::spawn_agent`.
    // `None` for plain `-p` one-shot runs.
    #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
) -> anyhow::Result<(String, rig::completion::Usage)>
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    run_print_with_stream_policy(
        agent,
        prompt,
        pure_stdout,
        retry_config,
        history,
        RunnerStreamPolicy::default(),
        #[cfg(feature = "hooks")]
        loop_info,
    )
    .await
}

async fn run_print_with_stream_policy<M>(
    agent: &Agent<M>,
    prompt: &str,
    pure_stdout: bool,
    retry_config: &RetryConfig,
    history: Vec<Message>,
    stream_policy: RunnerStreamPolicy,
    #[cfg(feature = "hooks")] loop_info: Option<LoopInfo>,
) -> anyhow::Result<(String, rig::completion::Usage)>
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let max_turns = agent.default_max_turns.unwrap_or(1);
    if max_turns == 0 {
        anyhow::bail!("Agent exhausted its maximum turn budget (0) before starting.");
    }

    let stream = retry::retry_stream_chat(retry_config, || {
        let p = prompt.to_string();
        let h = history.clone();
        async move { agent.stream_chat(p, h).max_turns(max_turns).await }
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut stream = stream_policy.apply(stream);

    let retry_history: Vec<Message> = history;
    let mut interactions: Vec<Message> = Vec::new();
    let mut continuation_bridge: Vec<Message> = Vec::new();
    let mut full_response = String::new();
    let mut response_len_at_stream_start = full_response.len();
    let mut tool_calls = ToolCallTracker::default();
    let mut completion_had_tool_call = false;
    let mut exhausted_budget_after_completion = None;
    let mut usage_ledger = UsageLedger::default();
    usage_ledger.start_stream();
    let mut turns_used = 0usize;
    let mut turns_at_stream_start = turns_used;
    // Drives the outer loop for either a `Stop`-forced continuation or recovery
    // from a provider stream that ended without a terminal response.
    let mut continue_turn = true;
    #[cfg(feature = "hooks")]
    let mut next_instruction: Option<String> = None;
    #[cfg(feature = "hooks")]
    let mut stop_hook_active = false;
    #[cfg(feature = "hooks")]
    let mut consecutive_stop_blocks: u32 = 0;
    #[cfg(feature = "hooks")]
    const MAX_STOP_BLOCKS: u32 = 8;

    while continue_turn {
        continue_turn = false;
        let mut terminal_response_seen = false;
        while let Some(item) = stream.next().await {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(
                    text,
                ))) => {
                    full_response.push_str(&text.text);
                    append_streamed_text(&mut interactions, &text.text);
                    print!("{}", text.text);
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
                Ok(MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::Reasoning(r),
                )) => {
                    eprint!("{}", r.display_text());
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                }
                Ok(MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ToolCall {
                        tool_call,
                        internal_call_id,
                    },
                )) => {
                    if let Some((used, budget)) = exhausted_budget_after_completion {
                        tool_calls.finalize_unresolved(&mut interactions);
                        anyhow::bail!(token_budget_exhaustion_message(used, budget));
                    }
                    let name = &tool_call.function.name;
                    if let Err(error) = tool_calls.record(&internal_call_id, &tool_call) {
                        tool_calls.finalize_unresolved(&mut interactions);
                        return Err(anyhow::anyhow!(error));
                    }
                    completion_had_tool_call = true;
                    if pure_stdout {
                        let summary = format_tool_args_summary(&tool_call.function.arguments);
                        println!("\n◈ {} {}", name, summary);
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    append_tool_call(&mut interactions, &tool_call);
                }
                Ok(MultiTurnStreamItem::ToolExecutionStart { .. }) => {
                    if let Some((used, budget)) = exhausted_budget_after_completion {
                        tool_calls.finalize_unresolved(&mut interactions);
                        anyhow::bail!(token_budget_exhaustion_message(used, budget));
                    }
                }
                Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    internal_call_id,
                })) => {
                    if let Some((used, budget)) = exhausted_budget_after_completion {
                        tool_calls.finalize_unresolved(&mut interactions);
                        anyhow::bail!(token_budget_exhaustion_message(used, budget));
                    }
                    let Some((name, output)) = append_attributed_tool_result(
                        &mut tool_calls,
                        &mut interactions,
                        &internal_call_id,
                        &tool_result,
                    ) else {
                        continue;
                    };
                    if pure_stdout && !output.is_empty() {
                        println!("◈ {} result:", name);
                        let lines: Vec<&str> = output.lines().collect();
                        if lines.len() > 40 {
                            let truncated: Vec<&str> = lines.iter().take(40).copied().collect();
                            println!("{}", truncated.join("\n"));
                            println!("(truncated {} more lines)", lines.len().saturating_sub(40));
                        } else {
                            println!("{}", output);
                        }
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                }
                Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                    turns_used = turns_used.saturating_add(1);
                    usage_ledger.record(call.usage);
                    tracing::debug!(
                        "agent completion: input_tokens={}, output_tokens={}, cumulative_tokens={}",
                        call.usage.input_tokens,
                        call.usage.output_tokens,
                        observed_tokens(usage_ledger.total),
                    );
                    if let Some((used, budget)) =
                        exhausted_token_budget(usage_ledger.total, agent.max_tokens)
                    {
                        if completion_had_tool_call {
                            tool_calls.finalize_unresolved(&mut interactions);
                            anyhow::bail!(token_budget_exhaustion_message(used, budget));
                        }
                        exhausted_budget_after_completion = Some((used, budget));
                    }
                    completion_had_tool_call = false;
                }
                Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                    terminal_response_seen = true;
                    usage_ledger.reconcile_terminal(res.usage());
                    reconcile_terminal_response(
                        &mut full_response,
                        response_len_at_stream_start,
                        &res.output,
                    );
                    if !tool_calls.finalize_unresolved(&mut interactions).is_empty() {
                        anyhow::bail!(UNRESOLVED_TOOL_CALLS_ERROR);
                    }
                    #[cfg(feature = "hooks")]
                    if let crate::extras::hooks::StopGate::Continue { reason } =
                        crate::extras::hooks::dispatch_stop(
                            stop_hook_active,
                            loop_info.map(|info| u64::from(info.iteration)),
                            loop_info.map(|info| info.active),
                        )
                        .await
                    {
                        consecutive_stop_blocks += 1;
                        if consecutive_stop_blocks <= MAX_STOP_BLOCKS {
                            stop_hook_active = true;
                            tracing::info!(
                                "hooks: Stop hook forced continuation ({consecutive_stop_blocks}/{MAX_STOP_BLOCKS}): {reason}"
                            );
                            next_instruction = Some(reason);
                            continue_turn = true;
                        } else {
                            tracing::warn!(
                                "hooks: Stop block cap ({MAX_STOP_BLOCKS}) reached without progress; forcing release"
                            );
                        }
                    }
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    // Propagate the stream failure instead of returning `Ok`
                    // with a truncated/empty response: dispatch must exit
                    // non-zero and must never persist an empty assistant turn
                    // (which would then be replayed as history on `--continue`).
                    tool_calls.finalize_unresolved(&mut interactions);
                    return Err(anyhow::anyhow!("{e}"));
                }
            }
        }

        tool_calls.finalize_unresolved(&mut interactions);

        if !terminal_response_seen {
            charge_nonterminal_eof(&mut turns_used, turns_at_stream_start, max_turns)
                .map_err(anyhow::Error::new)?;
            continue_turn = true;
        }

        if continue_turn {
            let remaining_turns = max_turns.saturating_sub(turns_used);
            if remaining_turns == 0 {
                anyhow::bail!(
                    "Agent exhausted its maximum turn budget ({max_turns}) before completing."
                );
            }
            if let Some((used, budget)) =
                exhausted_token_budget(usage_ledger.total, agent.max_tokens)
            {
                anyhow::bail!(
                    "Agent exhausted its cumulative token budget ({used}/{budget}) before \
                     completing. Compact the session or increase max_tokens before retrying."
                );
            }
            #[cfg(feature = "hooks")]
            let continuation_instruction = next_instruction
                .take()
                .unwrap_or_else(|| "Please continue.".to_string());
            #[cfg(not(feature = "hooks"))]
            let continuation_instruction = "Please continue.".to_string();
            let new_interactions = take_new_interactions(&mut interactions);
            continuation_bridge = normalized_continuation_bridge(
                &continuation_bridge,
                &new_interactions,
                &continuation_instruction,
            );
            // Keep the text already streamed to stdout this turn: the caller
            // persists the returned string as the assistant message, so
            // clearing it here would drop turn-1 output the user already saw
            // and desync the saved transcript from the terminal.
            stream = stream_policy.apply(
                continue_prompt_injector(
                    agent,
                    prompt,
                    &retry_history,
                    &continuation_bridge,
                    retry_config,
                    remaining_turns,
                )
                .await,
            );
            usage_ledger.start_stream();
            turns_at_stream_start = turns_used;
            response_len_at_stream_start = full_response.len();
        }
    }

    println!();
    Ok((full_response, usage_ledger.total))
}

fn format_tool_args_summary(args_json: &serde_json::Value) -> String {
    match args_json {
        serde_json::Value::Object(obj) => {
            let first_key = [
                "path",
                "file_path",
                "pattern",
                "command",
                "description",
                "content",
                "name",
                "question",
                "prompt",
            ];
            for key in &first_key {
                if let Some(val) = obj.get(*key) {
                    let s = match val {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let truncated: String = if s.len() > 120 {
                        // char-boundary-safe truncation for non-ASCII
                        let mut end = 117;
                        while !s.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}...", &s[..end])
                    } else {
                        s
                    };
                    return truncated.to_string();
                }
            }
            String::new()
        }
        _ => format!("{}", args_json),
    }
}

/// Run an agent silently (no stdout/stderr printing), collecting the full
/// response text. Used by subagent tasks.
#[cfg(feature = "subagents")]
pub(crate) struct SubagentRunOutput {
    pub response: Result<String, String>,
    pub usage: Usage,
}

#[cfg(feature = "subagents")]
pub(crate) async fn run_subagent<M>(
    agent: &Agent<M>,
    prompt: &str,
    max_turns: usize,
    event_tx: Option<&mpsc::Sender<AgentEvent>>,
    retry_config: &RetryConfig,
    usage_ledger: SharedUsageLedger,
) -> SubagentRunOutput
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let stream = retry::retry_stream_chat(retry_config, || {
        let p = prompt.to_string();
        async move {
            agent
                .stream_chat(p, Vec::<Message>::new())
                .max_turns(max_turns)
                .await
        }
    })
    .await;
    let mut stream = match stream {
        Ok(stream) => stream,
        Err(error) => {
            return SubagentRunOutput {
                response: Err(format!("subagent error: {error}")),
                usage: usage_ledger.total(),
            };
        }
    };

    let mut full_response = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                full_response.push_str(&text.text);
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                tool_call,
                ..
            })) => {
                if let Some(tx) = event_tx {
                    let _ = tx
                        .send(AgentEvent::SubagentToolCall {
                            name: CompactString::from(tool_call.function.name),
                            args: tool_call.function.arguments,
                        })
                        .await;
                }
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                usage_ledger.reconcile_terminal(res.usage());
                full_response = res.output.to_string();
                break;
            }
            Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                usage_ledger.record_completion(call.usage);
            }
            Ok(_) => {}
            Err(e) => {
                return SubagentRunOutput {
                    response: Err(format!("subagent error: {e}")),
                    usage: usage_ledger.total(),
                };
            }
        }
    }

    if full_response.is_empty() {
        return SubagentRunOutput {
            response: Err("subagent returned empty response".to_string()),
            usage: usage_ledger.total(),
        };
    }

    SubagentRunOutput {
        response: Ok(full_response),
        usage: usage_ledger.total(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PENDING_TOOL_CALLS, NonTerminalStreamExhausted, RunnerStreamPolicy, ToolCallTracker,
        ToolCallTrackerError, UNKNOWN_TOOL_OUTCOME, UNRESOLVED_TOOL_CALLS_ERROR, UsageLedger,
        append_attributed_tool_result, attributed_tool_result, charge_nonterminal_eof,
        completed_stream_delta, streamed_reasoning_text, warn_unknown_stream_item,
    };
    use futures::StreamExt;
    use rig::OneOrMany;
    use rig::agent::{
        AgentBuilder, AgentHook, Flow, HookContext, MultiTurnStreamItem, OutputMode, StepEvent,
    };
    use rig::completion::{CompletionModel, Message, Usage};
    use rig::message::{
        AssistantContent, Image, Text, ToolCall, ToolFunction, ToolResult, ToolResultContent,
        UserContent,
    };
    use rig::streaming::{StreamedAssistantContent, StreamingChat};
    use rig::test_utils::{MockCompletionModel, MockStreamEvent, MockToolError};
    use rig::tool::Tool;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RetryInvalidTool;

    #[tokio::test]
    async fn paused_runner_publishes_abort_before_model_work_can_start() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let paused = super::spawn_agent_paused(
            AgentBuilder::new(model.clone()).build(),
            "start only after publication".to_owned(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        tokio::task::yield_now().await;
        assert!(
            model.requests().is_empty(),
            "the model must remain unreachable behind the start barrier"
        );

        let cancellation = paused.cancellation_handle();
        cancellation.cancel();
        let mut runner = paused.start();
        assert!(runner.event_rx.recv().await.is_none());
        assert!(
            model.requests().is_empty(),
            "aborting before start must never enter provider work"
        );
    }

    impl<M: CompletionModel> AgentHook<M> for RetryInvalidTool {
        async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
            if matches!(event, StepEvent::InvalidToolCall(_)) {
                Flow::retry("normalized invalid-tool retry sentinel")
            } else {
                Flow::cont()
            }
        }
    }

    #[cfg(feature = "subagents")]
    #[derive(Clone)]
    struct RoutingProbeTool {
        marker: &'static str,
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[cfg(feature = "subagents")]
    impl Tool for RoutingProbeTool {
        const NAME: &'static str = "routing_probe";
        type Error = MockToolError;
        type Args = serde_json::Value;
        type Output = String;

        fn description(&self) -> String {
            "Emit a marker through the current runner's subagent event sender".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
            self.barrier.wait().await;
            let event_tx = crate::extras::subagents::clone_subagent_event_tx()
                .expect("spawn_agent must scope a subagent event sender");
            event_tx
                .send(crate::event::AgentEvent::SubagentToolCall {
                    name: self.marker.into(),
                    args: serde_json::json!({}),
                })
                .await
                .expect("runner event receiver must remain open");
            Ok(self.marker.to_string())
        }
    }

    #[derive(Clone)]
    struct CountingTool(Arc<AtomicUsize>);

    #[derive(Clone)]
    struct ScopedBlockingTool;

    impl Tool for ScopedBlockingTool {
        const NAME: &'static str = "scoped_blocking";
        type Error = MockToolError;
        type Args = serde_json::Value;
        type Output = String;

        fn description(&self) -> String {
            "Run owned blocking work".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
            super::spawn_blocking_scoped(|| "blocking-complete".to_string())
                .await
                .map_err(|_| MockToolError)
        }
    }

    impl Tool for CountingTool {
        const NAME: &'static str = "count";
        type Error = MockToolError;
        type Args = serde_json::Value;
        type Output = String;

        fn description(&self) -> String {
            "Count one invocation".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("counted".to_string())
        }
    }

    #[tokio::test]
    async fn cancellation_reaps_actual_agent_builder_blocking_tool_before_channel_close() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::tool_call(
                "blocking-call",
                ScopedBlockingTool::NAME,
                serde_json::json!({}),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = AgentBuilder::new(model)
            .tool(ScopedBlockingTool)
            .default_max_turns(2)
            .build();
        let (work_scope, started_rx, release) =
            super::AgentWorkScope::new_with_blocking_test_gate();
        let paused = super::spawn_agent_paused_in_scope(
            agent,
            "invoke the blocking tool".to_owned(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
            work_scope,
        );
        let cancellation = paused.cancellation_handle();
        let mut runner = paused.start();
        tokio::task::spawn_blocking(move || {
            started_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("the Rig tool should enter owned blocking work")
        })
        .await
        .unwrap();

        cancellation.cancel();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), async {
                while runner.event_rx.recv().await.is_some() {}
            })
            .await
            .is_err(),
            "the event channel must remain open while blocking tool work is live"
        );

        release.release();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while runner.event_rx.recv().await.is_some() {}
        })
        .await
        .expect("the event channel should close after blocking tool work is reaped");
    }

    #[cfg(all(feature = "hooks", unix))]
    #[tokio::test]
    async fn abort_handle_reaps_configured_async_hook_before_channel_close() {
        use crate::extras::hooks::dispatcher::HookDispatcher;
        use crate::extras::hooks::settings::{HookGroup, HookHandler, HooksConfig};

        let directory =
            std::env::temp_dir().join(format!("mini-agent-abort-hook-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let pid_file = directory.join("hook.pid");
        let mut config = HooksConfig::new();
        config.insert(
            "UserPromptSubmit".to_owned(),
            vec![HookGroup {
                matcher: None,
                hooks: vec![HookHandler {
                    kind: "command".to_owned(),
                    command: Some("/bin/sh".to_owned()),
                    args: Some(vec![
                        "-c".to_owned(),
                        "printf '%s' \"$$\" > \"$1\"; while :; do sleep 1; done".to_owned(),
                        "abort-hook".to_owned(),
                        pid_file.display().to_string(),
                    ]),
                    timeout: Some(60),
                    is_async: true,
                    condition: None,
                    once: false,
                    trust: Default::default(),
                    env: Default::default(),
                }],
            }],
        );
        let dispatcher = HookDispatcher::from_config(&config).unwrap();
        let ctx = crate::extras::hooks::HookCtx {
            session_id: "abort-handle-test".to_owned(),
            session_path: String::new(),
            cwd: directory.display().to_string(),
            permission_mode: "deny".to_owned(),
        };
        let (work_scope, started_rx, release) =
            super::AgentWorkScope::new_with_blocking_test_gate();
        let gate = work_scope
            .run(crate::extras::hooks::gate_user_prompt(
                &dispatcher,
                &ctx,
                "run blocking tool".to_owned(),
            ))
            .await;
        assert!(matches!(gate, crate::extras::hooks::PromptGate::Proceed(_)));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !pid_file.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("configured async hook should start");
        let hook_pid = std::fs::read_to_string(&pid_file).unwrap();

        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::tool_call(
                "blocking-call",
                ScopedBlockingTool::NAME,
                serde_json::json!({}),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = AgentBuilder::new(model)
            .tool(ScopedBlockingTool)
            .default_max_turns(2)
            .build();
        let paused = super::spawn_agent_paused_in_scope(
            agent,
            "invoke blocking tool".to_owned(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
            Arc::clone(&work_scope),
        );
        let mut runner = paused.start();
        tokio::task::spawn_blocking(move || {
            started_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("runner should enter scoped blocking tool work");
        })
        .await
        .unwrap();

        runner.abort_handle.abort();
        let closed_early = tokio::time::timeout(std::time::Duration::from_millis(50), async {
            while runner.event_rx.recv().await.is_some() {}
        })
        .await
        .is_ok();
        release.release();
        if closed_early {
            // Keep the regression failure hygienic under the old behavior.
            work_scope.cancellation_handle().cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), work_scope.wait_idle())
                .await;
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while runner.event_rx.recv().await.is_some() {}
            })
            .await
            .expect("event channel should close after hard-abort cleanup");
        }

        let hook_is_live = std::process::Command::new("kill")
            .args(["-0", hook_pid.trim()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        let _ = std::fs::remove_dir_all(directory);
        assert!(
            !closed_early,
            "AbortHandle must keep the event channel open through owned cleanup"
        );
        assert!(
            !hook_is_live,
            "hard-abort settlement left the hook process live"
        );
    }

    #[test]
    fn unknown_stream_item_warning_includes_item_information() {
        #[derive(Clone)]
        struct BufferWriter(Arc<std::sync::Mutex<Vec<u8>>>);

        impl Write for BufferWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("log buffer lock")
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let output = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = BufferWriter(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let item = MultiTurnStreamItem::<serde_json::Value>::StreamAssistantItem(
            StreamedAssistantContent::Unknown(serde_json::json!({
                "type": "cacheStatus",
                "cached": true,
            })),
        );

        tracing::subscriber::with_default(subscriber, || warn_unknown_stream_item(&item));

        let output = output.lock().expect("log buffer lock");
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("unknown stream item variant"));
        assert!(output.contains("cacheStatus"));
    }

    #[cfg(feature = "subagents")]
    #[tokio::test]
    async fn concurrent_agents_route_subagent_events_to_their_own_runner() {
        async fn collect_subagent_markers(
            mut runner: super::AgentRunner,
        ) -> Vec<compact_str::CompactString> {
            let mut markers = Vec::new();
            while let Some(event) = runner.event_rx.recv().await {
                match event {
                    crate::event::AgentEvent::SubagentToolCall { name, .. } => markers.push(name),
                    crate::event::AgentEvent::Done { .. } | crate::event::AgentEvent::Error(_) => {
                        break;
                    }
                    _ => {}
                }
            }
            markers
        }

        fn probe_agent(
            marker: &'static str,
            barrier: Arc<tokio::sync::Barrier>,
        ) -> rig::agent::Agent<MockCompletionModel> {
            let model = MockCompletionModel::from_stream_turns(vec![vec![
                MockStreamEvent::tool_call(
                    format!("{marker}-call"),
                    RoutingProbeTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ]]);
            AgentBuilder::new(model)
                .tool(RoutingProbeTool { marker, barrier })
                .default_max_turns(2)
                .build()
        }

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let runner_one = super::spawn_agent(
            probe_agent("runner-one", barrier.clone()),
            "start one".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let runner_two = super::spawn_agent(
            probe_agent("runner-two", barrier),
            "start two".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let (markers_one, markers_two) =
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::join!(
                    collect_subagent_markers(runner_one),
                    collect_subagent_markers(runner_two)
                )
            })
            .await
            .expect("concurrent agent runs must complete without deadlocking");

        assert_eq!(markers_one, ["runner-one"]);
        assert_eq!(markers_two, ["runner-two"]);
    }

    #[tokio::test]
    async fn continuation_streams_share_the_original_turn_budget() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tool = CountingTool(calls.clone());
        let mut turns = Vec::new();
        for turn in 0..10 {
            let events = if matches!(turn, 2 | 4 | 9) {
                vec![MockStreamEvent::final_response_with_default_usage()]
            } else {
                vec![
                    MockStreamEvent::tool_call(
                        format!("tool-{turn}"),
                        CountingTool::NAME,
                        serde_json::json!({}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ]
            };
            turns.push(events);
        }
        let model = MockCompletionModel::from_stream_turns(turns);
        let agent = AgentBuilder::new(model.clone())
            .tool(tool)
            .default_max_turns(5)
            .build();

        let mut runner = super::spawn_agent(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        while let Some(event) = runner.event_rx.recv().await {
            if matches!(
                event,
                crate::event::AgentEvent::Done { .. } | crate::event::AgentEvent::Error(_)
            ) {
                break;
            }
        }

        assert_eq!(
            model.requests().len(),
            5,
            "continuations must not reset the five-call model budget"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "only tool calls within the original budget may execute"
        );
    }

    #[tokio::test]
    async fn later_continuations_preserve_each_prior_tool_interaction_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("tool-first", CountingTool::NAME, serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![MockStreamEvent::final_response_with_default_usage()],
            vec![
                MockStreamEvent::tool_call(
                    "tool-second",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![MockStreamEvent::final_response_with_default_usage()],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .tool(CountingTool(calls))
            .default_max_turns(5)
            .build();

        let mut runner = super::spawn_agent(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        while let Some(event) = runner.event_rx.recv().await {
            if matches!(
                event,
                crate::event::AgentEvent::Done { .. } | crate::event::AgentEvent::Error(_)
            ) {
                break;
            }
        }

        let requests = model.requests();
        let request_tool_call_ids = |request_index: usize| {
            requests[request_index]
                .chat_history
                .iter()
                .flat_map(|message| match message {
                    Message::Assistant { content, .. } => content
                        .iter()
                        .filter_map(|content| match content {
                            AssistantContent::ToolCall(tool_call) => Some(tool_call.id.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(requests.len(), 5);
        assert_eq!(request_tool_call_ids(2), ["tool-first"]);
        assert_eq!(
            request_tool_call_ids(4),
            ["tool-first", "tool-second"],
            "the second continuation must preserve the full causal transcript without duplicates"
        );
    }

    #[tokio::test]
    async fn done_exposes_batched_tool_interactions_with_provider_ids() {
        let scripted_model = || {
            MockCompletionModel::from_stream_turns(vec![
                vec![
                    MockStreamEvent::tool_call(
                        "batch-first",
                        CountingTool::NAME,
                        serde_json::json!({}),
                    ),
                    MockStreamEvent::tool_call(
                        "batch-second",
                        CountingTool::NAME,
                        serde_json::json!({}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("finished"),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ])
        };

        let expected_agent = AgentBuilder::new(scripted_model())
            .tool(CountingTool(Arc::new(AtomicUsize::new(0))))
            .default_max_turns(3)
            .build();
        let mut expected_stream = expected_agent
            .stream_chat("run both", Vec::<Message>::new())
            .max_turns(3)
            .await;
        let expected_interactions = loop {
            match expected_stream.next().await {
                Some(Ok(MultiTurnStreamItem::FinalResponse(response))) => {
                    break completed_stream_delta(response.messages, "run both").unwrap();
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("Rig oracle failed: {error}"),
                None => panic!("Rig oracle ended without FinalResponse"),
            }
        };

        let calls = Arc::new(AtomicUsize::new(0));
        let model = scripted_model();
        let agent = AgentBuilder::new(model)
            .tool(CountingTool(calls.clone()))
            .default_max_turns(3)
            .build();
        let mut runner = super::spawn_agent(
            agent,
            "run both".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let interactions = loop {
            match runner.event_rx.recv().await {
                Some(crate::event::AgentEvent::Done { interactions, .. }) => break interactions,
                Some(crate::event::AgentEvent::Error(error)) => {
                    panic!("batched tool run failed: {error}")
                }
                Some(_) => {}
                None => panic!("batched tool run ended without Done"),
            }
        };

        let tool_call_ids = interactions
            .iter()
            .flat_map(|message| match message {
                Message::Assistant { content, .. } => content
                    .iter()
                    .filter_map(|item| match item {
                        AssistantContent::ToolCall(call) => Some(call.id.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        let tool_result_ids = interactions
            .iter()
            .flat_map(|message| match message {
                Message::User { content } => content
                    .iter()
                    .filter_map(|item| match item {
                        rig::message::UserContent::ToolResult(result) => Some(result.id.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(interactions, expected_interactions);
        assert_eq!(interactions.len(), 3, "parallel calls/results stay grouped");
        assert_eq!(tool_call_ids, ["batch-first", "batch-second"]);
        assert_eq!(tool_result_ids, tool_call_ids);
        assert_eq!(interactions.last(), Some(&Message::assistant("finished")));
    }

    #[tokio::test]
    async fn duplicate_provider_tool_ids_keep_distinct_lifecycle_ids() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "duplicate-provider-id",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::tool_call(
                    "duplicate-provider-id",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("finished"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model)
            .tool(CountingTool(calls.clone()))
            .default_max_turns(3)
            .build();
        let mut runner = super::spawn_agent(
            agent,
            "run duplicates".to_owned(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let mut call_lifecycle_ids = Vec::new();
        let mut result_lifecycle_ids = Vec::new();
        let interactions = loop {
            match runner.event_rx.recv().await {
                Some(crate::event::AgentEvent::ToolCall { id, .. }) => call_lifecycle_ids.push(id),
                Some(crate::event::AgentEvent::ToolResult { id, .. }) => {
                    result_lifecycle_ids.push(id)
                }
                Some(crate::event::AgentEvent::Done { interactions, .. }) => break interactions,
                Some(crate::event::AgentEvent::Error(error)) => {
                    panic!("duplicate-ID run failed: {error}")
                }
                Some(_) => {}
                None => panic!("duplicate-ID run ended without Done"),
            }
        };

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(call_lifecycle_ids.len(), 2);
        assert_ne!(call_lifecycle_ids[0], call_lifecycle_ids[1]);
        assert_eq!(result_lifecycle_ids, call_lifecycle_ids);
        let canonical_ids = interactions
            .iter()
            .flat_map(|message| match message {
                Message::Assistant { content, .. } => content
                    .iter()
                    .filter_map(|item| match item {
                        AssistantContent::ToolCall(call) => Some(call.id.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            canonical_ids,
            ["duplicate-provider-id", "duplicate-provider-id"]
        );
    }

    #[tokio::test]
    async fn completed_delta_preserves_rig_invalid_tool_retry_feedback_exactly() {
        let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("invalid-call", "missing_tool", serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("recovered"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]))
        .build();
        let mut stream = agent
            .runner("recover invalid tool")
            .max_turns(3)
            .max_invalid_tool_call_retries(1)
            .add_hook(RetryInvalidTool)
            .stream()
            .await;
        let authoritative = loop {
            match stream.next().await {
                Some(Ok(MultiTurnStreamItem::FinalResponse(response))) => {
                    break response.messages.expect("Rig final messages");
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("invalid-tool recovery failed: {error}"),
                None => panic!("invalid-tool recovery ended without FinalResponse"),
            }
        };
        let delta =
            completed_stream_delta(Some(authoritative.clone()), "recover invalid tool").unwrap();

        assert_eq!(delta, authoritative[1..]);
        assert!(
            serde_json::to_string(&delta)
                .unwrap()
                .contains("normalized invalid-tool retry sentinel"),
            "Rig's normalized retry feedback must survive unchanged"
        );
    }

    #[tokio::test]
    async fn done_matches_rig_output_retry_messages_exactly() {
        let scripted_model = || {
            MockCompletionModel::from_stream_turns(vec![
                vec![
                    MockStreamEvent::tool_call(
                        "output-invalid",
                        "final_result",
                        serde_json::json!({}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::tool_call(
                        "output-valid",
                        "final_result",
                        serde_json::json!({"answer": "done"}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ])
        };
        let build_agent = |model| {
            let output_schema: rig::schemars::Schema = serde_json::from_value(serde_json::json!({
                "type": "object",
                "required": ["answer"],
                "properties": {"answer": {"type": "string"}}
            }))
            .unwrap();
            AgentBuilder::new(model)
                .output_schema_raw(output_schema)
                .output_mode(OutputMode::Tool)
                .default_max_turns(3)
                .build()
        };

        let expected_agent = build_agent(scripted_model());
        let mut expected_stream = expected_agent
            .stream_chat("structured answer", Vec::<Message>::new())
            .max_turns(3)
            .await;
        let expected = loop {
            match expected_stream.next().await {
                Some(Ok(MultiTurnStreamItem::FinalResponse(response))) => {
                    break completed_stream_delta(response.messages, "structured answer").unwrap();
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("Rig output-retry oracle failed: {error}"),
                None => panic!("Rig output-retry oracle ended without FinalResponse"),
            }
        };

        let mut runner = super::spawn_agent(
            build_agent(scripted_model()),
            "structured answer".to_owned(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let actual = loop {
            match runner.event_rx.recv().await {
                Some(crate::event::AgentEvent::Done { interactions, .. }) => break interactions,
                Some(crate::event::AgentEvent::Error(error)) => {
                    panic!("output-retry runner failed: {error}")
                }
                Some(_) => {}
                None => panic!("output-retry runner ended without Done"),
            }
        };

        assert_eq!(actual, expected);
        assert!(actual.len() >= 3, "retry feedback must be retained");
    }

    #[tokio::test]
    async fn continuation_stops_when_cumulative_token_budget_is_exhausted() {
        fn usage(input_tokens: u64, output_tokens: u64) -> Usage {
            Usage {
                input_tokens,
                output_tokens,
                total_tokens: input_tokens + output_tokens,
                ..Usage::new()
            }
        }

        let model = MockCompletionModel::from_stream_turns(vec![
            vec![MockStreamEvent::final_response(usage(40, 15))],
            vec![MockStreamEvent::final_response(usage(40, 15))],
            vec![MockStreamEvent::final_response(usage(40, 15))],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .max_tokens(100)
            .default_max_turns(5)
            .build();

        let mut runner = super::spawn_agent(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let mut error = None;
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Error(message) => {
                    error = Some(message);
                    break;
                }
                crate::event::AgentEvent::Done { .. } => break,
                _ => {}
            }
        }

        assert_eq!(
            model.requests().len(),
            2,
            "the third continuation must not start after 110/100 cumulative tokens"
        );
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.contains("110/100")),
            "the runner must report cumulative token-budget exhaustion"
        );
    }

    #[test]
    fn runner_eof_without_terminal_event_repeated_immediate_eof_is_bounded() {
        let max_turns = 3;
        let mut turns_used = 0;
        let mut provider_calls = 0;

        let error = loop {
            provider_calls += 1;
            let turns_at_stream_start = turns_used;
            match charge_nonterminal_eof(&mut turns_used, turns_at_stream_start, max_turns) {
                Ok(()) => continue,
                Err(error) => break error,
            }
        };

        assert_eq!(
            provider_calls, max_turns,
            "EOF retries must stop exactly at the shared turn limit"
        );
        assert_eq!(
            error,
            NonTerminalStreamExhausted {
                attempts: 3,
                limit: 3,
            }
        );
        assert_eq!(
            error.to_string(),
            "Agent stream ended without a terminal response; provider attempt budget exhausted (3/3)."
        );
    }

    #[test]
    fn runner_eof_without_terminal_event_partial_text_still_consumes_attempt() {
        let mut turns_used = 0;
        let turns_at_stream_start = turns_used;
        let partial_text = "provider output that must not appear in the diagnostic";

        let error = charge_nonterminal_eof(&mut turns_used, turns_at_stream_start, 1)
            .expect_err("a partial nonterminal stream still exhausts a one-turn budget");

        assert_eq!(turns_used, 1);
        assert!(!error.to_string().contains(partial_text));
    }

    #[test]
    fn runner_eof_without_terminal_event_malformed_item_does_not_double_charge_completion() {
        let turns_at_stream_start = 4;
        let mut turns_used = 5;

        charge_nonterminal_eof(&mut turns_used, turns_at_stream_start, 6)
            .expect("the already-accounted attempt remains within budget");

        assert_eq!(
            turns_used, 5,
            "an observed CompletionCall is the same provider attempt"
        );
    }

    #[test]
    fn runner_eof_without_terminal_event_transient_eof_leaves_room_for_completion() {
        let mut turns_used = 0;
        let turns_at_stream_start = turns_used;

        charge_nonterminal_eof(&mut turns_used, turns_at_stream_start, 2)
            .expect("one transient EOF may recover within the shared budget");

        assert_eq!(turns_used, 1);
        // A terminal response on the next provider call bypasses EOF charging;
        // normal CompletionCall accounting consumes the remaining turn.
        turns_used += 1;
        assert_eq!(turns_used, 2);
    }

    #[test]
    fn runner_eof_without_terminal_event_tool_call_and_result_then_eof_is_bounded() {
        // A tool-bearing provider call normally emits CompletionCall before its
        // ToolCall/ToolResult pair. EOF after those events must use that same
        // charged turn rather than adding or losing an attempt.
        let turns_at_stream_start = 0;
        let mut turns_used = 1;

        let error = charge_nonterminal_eof(&mut turns_used, turns_at_stream_start, 1)
            .expect_err("the already-charged tool turn exhausted the shared budget");

        assert_eq!(turns_used, 1);
        assert_eq!(error.attempts, 1);
        assert_eq!(error.limit, 1);
    }

    #[test]
    fn runner_eof_without_terminal_event_interactive_headless_diagnostic_parity() {
        let exhaust = || {
            let mut turns_used = 0;
            charge_nonterminal_eof(&mut turns_used, 0, 1)
                .expect_err("one-turn EOF must exhaust")
                .to_string()
        };

        let interactive_diagnostic = exhaust();
        let headless_diagnostic = anyhow::Error::new({
            let mut turns_used = 0;
            charge_nonterminal_eof(&mut turns_used, 0, 1).expect_err("one-turn EOF must exhaust")
        })
        .to_string();

        assert_eq!(interactive_diagnostic, headless_diagnostic);
        assert_eq!(
            interactive_diagnostic,
            "Agent stream ended without a terminal response; provider attempt budget exhausted (1/1)."
        );
    }

    #[tokio::test]
    async fn runner_eof_without_terminal_event_terminal_completion_is_unchanged() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = AgentBuilder::new(model.clone())
            .default_max_turns(2)
            .build();
        let mut runner = super::spawn_agent(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let mut response = None;
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Done { response: done, .. } => {
                    response = Some(done.to_string());
                    break;
                }
                crate::event::AgentEvent::Error(error) => panic!("unexpected error: {error}"),
                _ => {}
            }
        }

        assert_eq!(model.requests().len(), 1);
        assert_eq!(response.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn runner_eof_without_terminal_event_valid_tool_continuation_is_unchanged() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("tool-ok", CountingTool::NAME, serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("finished"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .tool(CountingTool(calls.clone()))
            .default_max_turns(2)
            .build();
        let mut runner = super::spawn_agent(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let mut response = None;
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Done { response: done, .. } => {
                    response = Some(done.to_string());
                    break;
                }
                crate::event::AgentEvent::Error(error) => panic!("unexpected error: {error}"),
                _ => {}
            }
        }

        assert_eq!(model.requests().len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(response.as_deref(), Some("finished"));
    }

    #[tokio::test]
    async fn runner_eof_without_terminal_event_repeated_immediate_eof_stops_exactly_at_limit() {
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![MockStreamEvent::final_response_with_default_usage()],
            vec![MockStreamEvent::final_response_with_default_usage()],
            vec![MockStreamEvent::final_response_with_default_usage()],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .default_max_turns(3)
            .build();
        let mut runner = super::spawn_agent_with_stream_policy(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            RunnerStreamPolicy::drop_next_terminal_responses(3),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let mut error = None;
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Error(message) => {
                    error = Some(message.to_string());
                    break;
                }
                crate::event::AgentEvent::Done { response, .. } => {
                    panic!("unexpected completion after repeated EOF: {response}")
                }
                _ => {}
            }
        }

        assert_eq!(model.requests().len(), 3);
        assert_eq!(
            error.as_deref(),
            Some(
                "Agent stream ended without a terminal response; provider attempt budget exhausted (3/3)."
            )
        );
    }

    #[tokio::test]
    async fn runner_eof_without_terminal_event_partial_text_is_persisted_and_replayed() {
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::text("prefix "),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("suffix"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .default_max_turns(2)
            .build();
        let mut runner = super::spawn_agent_with_stream_policy(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            RunnerStreamPolicy::drop_next_terminal_responses(1),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let mut streamed = String::new();
        let mut done = None;
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Token(text) => streamed.push_str(&text),
                crate::event::AgentEvent::Done { response, .. } => {
                    done = Some(response.to_string());
                    break;
                }
                crate::event::AgentEvent::Error(error) => panic!("unexpected error: {error}"),
                _ => {}
            }
        }

        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(streamed, "prefix suffix");
        assert_eq!(done.as_deref(), Some("prefix suffix"));
        assert_eq!(
            requests[1].chat_history.iter().cloned().collect::<Vec<_>>(),
            vec![
                Message::user("start"),
                Message::assistant("prefix "),
                Message::user("Please continue."),
            ],
            "continuation history must preserve causal prompt/output order"
        );
    }

    #[tokio::test]
    async fn runner_eof_without_terminal_event_tool_result_and_text_are_replayed_in_order() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "tool-before-eof",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("partial "),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("complete"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .tool(CountingTool(calls.clone()))
            .default_max_turns(3)
            .build();
        let mut runner = super::spawn_agent_with_stream_policy(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            RunnerStreamPolicy::drop_next_terminal_responses(1),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let mut done = None;
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Done { response, .. } => {
                    done = Some(response.to_string());
                    break;
                }
                crate::event::AgentEvent::Error(error) => panic!("unexpected error: {error}"),
                _ => {}
            }
        }

        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(done.as_deref(), Some("partial complete"));
        let continuation_history = &requests[2].chat_history;
        assert_eq!(continuation_history.len(), 5);
        assert_eq!(continuation_history.first_ref(), &Message::user("start"));
        let tool_call_index = continuation_history
            .iter()
            .position(|message| matches!(message, Message::Assistant { content, .. } if content.iter().any(|item| matches!(item, AssistantContent::ToolCall(call) if call.id == "tool-before-eof"))))
            .expect("continuation contains tool call");
        let tool_result_index = continuation_history
            .iter()
            .position(|message| matches!(message, Message::User { content } if content.iter().any(|item| matches!(item, rig::message::UserContent::ToolResult(result) if result.id == "tool-before-eof"))))
            .expect("continuation contains tool result");
        let partial_text_index = continuation_history
            .iter()
            .position(|message| matches!(message, Message::Assistant { content, .. } if content.iter().any(|item| matches!(item, AssistantContent::Text(text) if text.text == "partial "))))
            .expect("continuation contains partial text");
        assert_eq!(
            (tool_call_index, tool_result_index, partial_text_index),
            (1, 2, 3)
        );
        assert_eq!(
            continuation_history.last_ref(),
            &Message::user("Please continue.")
        );
    }

    #[tokio::test]
    async fn runner_tool_call_text_eof_preserves_one_assistant_turn_on_both_surfaces() {
        fn assert_recovery_history(history: Vec<Message>) -> ToolResult {
            assert_eq!(history.len(), 5);
            assert_eq!(history[0], Message::user("start"));
            let Message::Assistant { content, .. } = &history[1] else {
                panic!("tool call and following text must share one assistant turn")
            };
            assert_eq!(content.len(), 2);
            assert!(matches!(
                content.first(),
                AssistantContent::ToolCall(call) if call.id == "eof-provider-id"
            ));
            assert!(matches!(
                content.last(),
                AssistantContent::Text(text) if text.text == "between"
            ));
            let Message::User { content } = &history[2] else {
                panic!("synthetic tool result immediately follows the assistant turn")
            };
            let UserContent::ToolResult(result) = content.first() else {
                panic!("expected synthetic tool result")
            };
            assert_eq!(result.id, "eof-provider-id");
            assert!(result.content.iter().any(
                |part| matches!(part, ToolResultContent::Text(text) if text.text == UNKNOWN_TOOL_OUTCOME)
            ));
            assert!(matches!(
                &history[3],
                Message::Assistant { content, .. }
                    if matches!(content.first(), AssistantContent::Text(text) if text.text.is_empty())
            ));
            assert_eq!(history[4], Message::user("Please continue."));
            result.clone()
        }

        async fn run_interactive() -> (ToolResult, String, String) {
            let model = MockCompletionModel::from_stream_turns(vec![
                vec![
                    MockStreamEvent::tool_call(
                        "eof-provider-id",
                        CountingTool::NAME,
                        serde_json::json!({}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("between"),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("recovered"),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ]);
            let agent = AgentBuilder::new(model.clone())
                .tool(CountingTool(Arc::new(AtomicUsize::new(0))))
                .default_max_turns(3)
                .build();
            let mut runner = super::spawn_agent_with_stream_policy(
                agent,
                "start".to_string(),
                Vec::new(),
                crate::retry::RetryConfig::default(),
                RunnerStreamPolicy::tool_call_text_eof(),
                #[cfg(feature = "skills")]
                None,
                #[cfg(feature = "hooks")]
                None,
            );
            let mut call_id = None;
            let mut synthetic = None;
            while let Some(event) = runner.event_rx.recv().await {
                match event {
                    crate::event::AgentEvent::ToolCall { id, name, .. } => {
                        assert_eq!(name, CountingTool::NAME);
                        call_id = Some(id.to_string());
                    }
                    crate::event::AgentEvent::ToolResult { id, name, output } => {
                        assert_eq!(name, CountingTool::NAME);
                        assert_eq!(output, UNKNOWN_TOOL_OUTCOME);
                        synthetic = Some(id.to_string());
                    }
                    crate::event::AgentEvent::Done { response, .. } => {
                        assert_eq!(response, "betweenrecovered");
                        break;
                    }
                    crate::event::AgentEvent::Error(error) => {
                        panic!("unexpected interactive error: {error}")
                    }
                    _ => {}
                }
            }
            let requests = model.requests();
            assert_eq!(requests.len(), 3);
            let result = assert_recovery_history(
                requests[2].chat_history.iter().cloned().collect::<Vec<_>>(),
            );
            (result, call_id.unwrap(), synthetic.unwrap_or_default())
        }

        async fn run_headless() -> ToolResult {
            let model = MockCompletionModel::from_stream_turns(vec![
                vec![
                    MockStreamEvent::tool_call(
                        "eof-provider-id",
                        CountingTool::NAME,
                        serde_json::json!({}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("between"),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("recovered"),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ]);
            let agent = AgentBuilder::new(model.clone())
                .tool(CountingTool(Arc::new(AtomicUsize::new(0))))
                .default_max_turns(3)
                .build();
            let (response, _) = super::run_print_with_stream_policy(
                &agent,
                "start",
                false,
                &crate::retry::RetryConfig::default(),
                Vec::new(),
                RunnerStreamPolicy::tool_call_text_eof(),
                #[cfg(feature = "hooks")]
                None,
            )
            .await
            .expect("headless EOF recovery completes after synthesizing a result");
            assert_eq!(response, "betweenrecovered");
            let requests = model.requests();
            assert_eq!(requests.len(), 3);
            assert_recovery_history(requests[2].chat_history.iter().cloned().collect::<Vec<_>>())
        }

        let (interactive_result, call_id, result_id) = run_interactive().await;
        let headless_result = run_headless().await;
        assert_eq!(call_id, result_id);
        assert_eq!(interactive_result, headless_result);
        assert_eq!(interactive_result.id, "eof-provider-id");
        assert!(interactive_result.content.iter().any(
            |part| matches!(part, ToolResultContent::Text(text) if text.text == UNKNOWN_TOOL_OUTCOME)
        ));
    }

    #[tokio::test]
    async fn runner_final_response_with_pending_call_fails_closed_on_both_surfaces() {
        let interactive_model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "terminal-provider-id",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("must not succeed"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let interactive_agent = AgentBuilder::new(interactive_model)
            .tool(CountingTool(Arc::new(AtomicUsize::new(0))))
            .default_max_turns(2)
            .build();
        let mut runner = super::spawn_agent_with_stream_policy(
            interactive_agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            RunnerStreamPolicy::without_tool_results(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let mut call_id = None;
        let mut result_id = None;
        let interactive_error = loop {
            match runner.event_rx.recv().await.expect("runner terminal event") {
                crate::event::AgentEvent::ToolCall { id, .. } => call_id = Some(id.to_string()),
                crate::event::AgentEvent::ToolResult { id, name, output } => {
                    assert_eq!(name, CountingTool::NAME);
                    assert_eq!(output, UNKNOWN_TOOL_OUTCOME);
                    result_id = Some(id.to_string());
                }
                crate::event::AgentEvent::Error(error) => break error.to_string(),
                crate::event::AgentEvent::Done { .. } => {
                    panic!("pending calls must never produce interactive success")
                }
                _ => {}
            }
        };
        assert_eq!(call_id, result_id);
        assert_eq!(interactive_error, UNRESOLVED_TOOL_CALLS_ERROR);

        let headless_model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "terminal-provider-id",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("must not succeed"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let headless_agent = AgentBuilder::new(headless_model)
            .tool(CountingTool(Arc::new(AtomicUsize::new(0))))
            .default_max_turns(2)
            .build();
        let headless_error = super::run_print_with_stream_policy(
            &headless_agent,
            "start",
            false,
            &crate::retry::RetryConfig::default(),
            Vec::new(),
            RunnerStreamPolicy::without_tool_results(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .expect_err("pending calls must never produce headless success");
        assert_eq!(headless_error.to_string(), interactive_error);
    }

    #[tokio::test]
    async fn text_and_tool_eof_history_is_causal_and_protocol_complete() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::text("checking"),
                MockStreamEvent::tool_call(
                    "causal-tool",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![MockStreamEvent::final_response_with_default_usage()],
            vec![
                MockStreamEvent::text("answer"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .tool(CountingTool(calls.clone()))
            .default_max_turns(3)
            .build();
        let mut runner = super::spawn_agent_with_stream_policy(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            RunnerStreamPolicy::drop_next_terminal_responses(1),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let mut done = None;
        let mut done_interactions = None;
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Done {
                    response,
                    interactions,
                    ..
                } => {
                    done = Some(response.to_string());
                    done_interactions = Some(interactions);
                    break;
                }
                crate::event::AgentEvent::Error(error) => panic!("unexpected error: {error}"),
                _ => {}
            }
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(done.as_deref(), Some("answer"));
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        let history = &requests[2].chat_history;
        assert_eq!(history.len(), 5);
        assert_eq!(history.first_ref(), &Message::user("start"));
        assert!(matches!(
            history.iter().nth(1),
            Some(Message::Assistant { content, .. })
                if matches!(content.iter().next(), Some(AssistantContent::Text(text)) if text.text == "checking")
                    && matches!(content.iter().nth(1), Some(AssistantContent::ToolCall(call)) if call.id == "causal-tool")
                    && content.len() == 2
        ));
        assert!(matches!(
            history.iter().nth(2),
            Some(Message::User { content })
                if matches!(content.first_ref(), rig::message::UserContent::ToolResult(result) if result.id == "causal-tool")
        ));
        assert_eq!(history.iter().nth(3), Some(&Message::assistant("")));
        assert_eq!(history.last_ref(), &Message::user("Please continue."));
        let mut expected_interactions = history.iter().skip(1).cloned().collect::<Vec<_>>();
        expected_interactions.push(Message::assistant("answer"));
        assert_eq!(
            done_interactions.as_deref(),
            Some(expected_interactions.as_slice()),
            "the committed turn delta must be the exact model-visible continuation transcript"
        );
    }

    #[tokio::test]
    async fn runner_eof_without_terminal_event_headless_recovers_without_duplicate_text() {
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::text("prefix "),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("suffix"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .default_max_turns(2)
            .build();

        let (response, _) = super::run_print_with_stream_policy(
            &agent,
            "start",
            false,
            &crate::retry::RetryConfig::default(),
            Vec::new(),
            RunnerStreamPolicy::drop_next_terminal_responses(1),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .expect("headless EOF recovery succeeds");

        assert_eq!(model.requests().len(), 2);
        assert_eq!(response, "prefix suffix");
        assert_eq!(
            model.requests()[1]
                .chat_history
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                Message::user("start"),
                Message::assistant("prefix "),
                Message::user("Please continue."),
            ]
        );
    }

    #[tokio::test]
    async fn text_before_tool_and_final_answer_keep_session_chronology() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::text("checking"),
                MockStreamEvent::tool_call(
                    "chronology-tool",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("answer"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model)
            .tool(CountingTool(calls.clone()))
            .default_max_turns(2)
            .build();
        let mut runner = super::spawn_agent(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let mut session = crate::session::Session::new("test", "mock", 4096, "chronology");
        let mut buffered_segment = String::new();
        let mut rendered = String::new();
        let mut done = None;
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Token(text) => {
                    buffered_segment.push_str(&text);
                    rendered.push_str(&text);
                }
                crate::event::AgentEvent::ToolCall { id, name, args } => {
                    if !buffered_segment.is_empty() {
                        session
                            .add_message(crate::session::MessageRole::Assistant, &buffered_segment);
                        buffered_segment.clear();
                    }
                    session.add_tool_call_with_id(&id, &name, &args);
                }
                crate::event::AgentEvent::ToolResult { id, name, output } => {
                    session.add_tool_result_with_id(&id, &name, &output);
                }
                crate::event::AgentEvent::Done { response, .. } => {
                    session.add_message(crate::session::MessageRole::Assistant, &response);
                    done = Some(response.to_string());
                    break;
                }
                crate::event::AgentEvent::Error(error) => panic!("unexpected error: {error}"),
                _ => {}
            }
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(rendered, "checkinganswer");
        assert_eq!(done.as_deref(), Some("answer"));
        assert_eq!(
            session
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            vec![
                crate::session::MessageRole::Assistant,
                crate::session::MessageRole::ToolCall,
                crate::session::MessageRole::ToolResult,
                crate::session::MessageRole::Assistant,
            ]
        );
        assert_eq!(session.messages[0].content, "checking");
        assert_eq!(session.messages[3].content, "answer");
    }

    #[tokio::test]
    async fn duplicate_provider_tool_ids_emit_distinct_exact_internal_correlations() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("count", CountingTool::NAME, serde_json::json!({})),
                MockStreamEvent::tool_call("count", CountingTool::NAME, serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model)
            .tool(CountingTool(calls.clone()))
            .default_max_turns(2)
            .build();
        let mut runner = super::spawn_agent(
            agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let mut call_ids = Vec::new();
        let mut result_ids = Vec::new();
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::ToolCall { id, .. } => call_ids.push(id.to_string()),
                crate::event::AgentEvent::ToolResult { id, .. } => result_ids.push(id.to_string()),
                crate::event::AgentEvent::Done { .. } => break,
                crate::event::AgentEvent::Error(error) => panic!("unexpected error: {error}"),
                _ => {}
            }
        }

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(call_ids.len(), 2);
        assert_ne!(call_ids[0], call_ids[1]);
        call_ids.sort();
        result_ids.sort();
        assert_eq!(result_ids, call_ids);
    }

    #[tokio::test]
    async fn runner_eof_without_terminal_event_headless_exhaustion_has_exact_call_count() {
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![MockStreamEvent::final_response_with_default_usage()],
            vec![MockStreamEvent::final_response_with_default_usage()],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .default_max_turns(2)
            .build();

        let error = super::run_print_with_stream_policy(
            &agent,
            "start",
            false,
            &crate::retry::RetryConfig::default(),
            Vec::new(),
            RunnerStreamPolicy::drop_next_terminal_responses(2),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .expect_err("repeated headless EOF must exhaust");

        assert_eq!(model.requests().len(), 2);
        assert_eq!(
            error.to_string(),
            "Agent stream ended without a terminal response; provider attempt budget exhausted (2/2)."
        );
    }

    #[test]
    fn terminal_without_aggregate_does_not_erase_already_streamed_text() {
        let mut response = "prefix".to_string();

        super::reconcile_terminal_response(&mut response, 0, "");

        assert_eq!(response, "prefix");
    }

    #[tokio::test]
    async fn runner_zero_turn_budget_starts_no_provider_calls_on_both_surfaces() {
        let interactive_model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("must not run"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let interactive_agent = AgentBuilder::new(interactive_model.clone())
            .default_max_turns(0)
            .build();
        let mut runner = super::spawn_agent(
            interactive_agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let interactive_error = loop {
            match runner.event_rx.recv().await {
                Some(crate::event::AgentEvent::Error(error)) => break error.to_string(),
                Some(crate::event::AgentEvent::Done { response, .. }) => {
                    panic!("zero budget unexpectedly completed: {response}")
                }
                Some(_) => {}
                None => panic!("runner ended without a diagnostic"),
            }
        };

        let headless_model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("must not run"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let headless_agent = AgentBuilder::new(headless_model.clone())
            .default_max_turns(0)
            .build();
        let headless_error = super::run_print(
            &headless_agent,
            "start",
            false,
            &crate::retry::RetryConfig::default(),
            Vec::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .expect_err("zero headless budget must fail before starting");

        assert!(interactive_model.requests().is_empty());
        assert!(headless_model.requests().is_empty());
        assert_eq!(
            interactive_error,
            "Agent exhausted its maximum turn budget (0) before starting."
        );
        assert_eq!(headless_error.to_string(), interactive_error);
    }

    #[test]
    fn tool_result_without_preceding_tool_call_is_skipped() {
        let tool_result = ToolResult {
            id: "orphan-result".to_string(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::Text(Text::new("unexpected output"))),
        };
        let mut tracker = ToolCallTracker::default();

        assert!(attributed_tool_result(&mut tracker, "orphan-internal", &tool_result).is_none());
    }

    #[test]
    fn non_text_tool_result_has_visible_fallback_output() {
        let tool_result = ToolResult {
            id: "image-result".to_string(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::Image(Image::default())),
        };
        let mut tracker = ToolCallTracker::default();
        tracker
            .record(
                "image-internal",
                &ToolCall::new(
                    "image-result".to_string(),
                    ToolFunction::new("image_tool".to_string(), serde_json::json!({})),
                ),
            )
            .expect("call must fit in the tracker");

        let (tool_name, output) =
            attributed_tool_result(&mut tracker, "image-internal", &tool_result)
                .expect("a result with a preceding tool call must be attributed");

        assert_eq!(tool_name, "image_tool");
        assert_eq!(
            output,
            "[Tool result contained non-text content that cannot be displayed as text.]"
        );
    }

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall::new(
            id.to_string(),
            ToolFunction::new(name.to_string(), serde_json::json!({})),
        )
    }

    fn text_result(id: &str, output: &str) -> ToolResult {
        text_result_with_call_id(id, None, output)
    }

    fn text_result_with_call_id(id: &str, call_id: Option<&str>, output: &str) -> ToolResult {
        ToolResult {
            id: id.to_string(),
            call_id: call_id.map(str::to_string),
            content: OneOrMany::one(ToolResultContent::Text(Text::new(output))),
        }
    }

    #[test]
    fn runner_parallel_tool_result_correlation_handles_out_of_order_and_same_names() {
        let mut tracker = ToolCallTracker::default();
        tracker
            .record("internal-a", &tool_call("read", "read"))
            .unwrap();
        tracker
            .record("internal-b", &tool_call("bash", "bash"))
            .unwrap();
        tracker
            .record("internal-c", &tool_call("read", "read"))
            .unwrap();

        let (name_b, output_b) = attributed_tool_result(
            &mut tracker,
            "internal-b",
            &text_result("bash", "second finished first"),
        )
        .expect("call B must remain independently addressable");
        let (name_a, output_a) =
            attributed_tool_result(&mut tracker, "internal-a", &text_result("read", "first"))
                .expect("call A must not be overwritten by call B");
        let (name_c, output_c) =
            attributed_tool_result(&mut tracker, "internal-c", &text_result("read", "third"))
                .expect("same-name calls with duplicate provider IDs remain distinct internally");

        assert_eq!(
            (name_b.as_str(), output_b.as_str()),
            ("bash", "second finished first")
        );
        assert_eq!((name_a.as_str(), output_a.as_str()), ("read", "first"));
        assert_eq!((name_c.as_str(), output_c.as_str()), ("read", "third"));
    }

    #[test]
    fn runner_unknown_tool_result_does_not_consume_a_valid_pending_call() {
        let mut tracker = ToolCallTracker::default();
        tracker
            .record("valid-internal", &tool_call("valid-provider", "read"))
            .unwrap();

        assert!(
            attributed_tool_result(
                &mut tracker,
                "unknown-internal",
                &text_result("valid-provider", "ignored"),
            )
            .is_none()
        );
        assert!(
            attributed_tool_result(
                &mut tracker,
                "",
                &text_result("valid-provider", "missing ID"),
            )
            .is_none()
        );
        let valid = attributed_tool_result(
            &mut tracker,
            "valid-internal",
            &text_result("valid-provider", "kept"),
        )
        .expect("unknown and missing IDs must leave valid correlations intact");
        assert_eq!((valid.0.as_str(), valid.1.as_str()), ("read", "kept"));
        assert!(
            attributed_tool_result(
                &mut tracker,
                "valid-internal",
                &text_result("valid-provider", "duplicate"),
            )
            .is_none(),
            "an internal result ID must resolve at most once"
        );
    }

    #[test]
    fn mismatched_provider_result_id_preserves_the_valid_pending_call() {
        let mut tracker = ToolCallTracker::default();
        let mut interactions = Vec::new();
        tracker
            .record("stable-internal", &tool_call("expected-provider", "read"))
            .unwrap();

        assert!(
            append_attributed_tool_result(
                &mut tracker,
                &mut interactions,
                "stable-internal",
                &text_result("wrong-provider", "malformed"),
            )
            .is_none()
        );
        assert_eq!(tracker.pending.len(), 1);
        assert!(
            interactions.is_empty(),
            "a rejected provider result must not enter persisted history"
        );
        let finalized = tracker.finalize_unresolved(&mut interactions);
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].internal_call_id, "stable-internal");
        assert_eq!(finalized[0].name, "read");
        assert_eq!(finalized[0].tool_result.id, "expected-provider");
        assert!(
            finalized[0]
                .tool_result
                .content
                .iter()
                .any(|part| matches!(part, ToolResultContent::Text(text) if text.text.contains("outcome is unknown")))
        );
    }

    #[test]
    fn provider_call_id_is_strict_for_duplicate_provider_ids_and_optional_values() {
        let mut tracker = ToolCallTracker::default();
        let mut first = tool_call("duplicate-provider", "first");
        first.call_id = Some("call-a".to_string());
        let mut second = tool_call("duplicate-provider", "second");
        second.call_id = Some("call-b".to_string());
        tracker.record("internal-a", &first).unwrap();
        tracker.record("internal-b", &second).unwrap();
        tracker
            .record("internal-none", &tool_call("optional-provider", "third"))
            .unwrap();

        assert!(
            attributed_tool_result(
                &mut tracker,
                "internal-a",
                &text_result_with_call_id("duplicate-provider", Some("call-b"), "swapped",),
            )
            .is_none()
        );
        assert!(
            attributed_tool_result(
                &mut tracker,
                "internal-b",
                &text_result_with_call_id("duplicate-provider", Some("call-a"), "swapped",),
            )
            .is_none()
        );
        assert!(
            attributed_tool_result(
                &mut tracker,
                "internal-none",
                &text_result_with_call_id("optional-provider", Some("unexpected"), "mismatch",),
            )
            .is_none()
        );

        let mut interactions = Vec::new();
        let finalized = tracker.finalize_unresolved(&mut interactions);
        assert_eq!(
            finalized
                .iter()
                .map(|call| (
                    call.internal_call_id.as_str(),
                    call.tool_result.id.as_str(),
                    call.tool_result.call_id.as_deref(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("internal-a", "duplicate-provider", Some("call-a")),
                ("internal-b", "duplicate-provider", Some("call-b")),
                ("internal-none", "optional-provider", None),
            ]
        );
    }

    #[test]
    fn matching_provider_call_id_resolves_normally() {
        let mut tracker = ToolCallTracker::default();
        let mut call = tool_call("provider", "read");
        call.call_id = Some("provider-call".to_string());
        tracker.record("internal", &call).unwrap();

        let result = attributed_tool_result(
            &mut tracker,
            "internal",
            &text_result_with_call_id("provider", Some("provider-call"), "done"),
        )
        .expect("matching provider id and call_id resolve the exact call");

        assert_eq!((result.0.as_str(), result.1.as_str()), ("read", "done"));
        assert!(tracker.pending.is_empty());
        assert!(tracker.order.is_empty());
    }

    #[test]
    fn runner_duplicate_internal_tool_call_id_fails_without_replacing_the_original() {
        let mut tracker = ToolCallTracker::default();
        tracker
            .record("same-internal", &tool_call("provider-a", "original"))
            .unwrap();
        assert_eq!(
            tracker.record("same-internal", &tool_call("provider-b", "replacement")),
            Err(ToolCallTrackerError::DuplicateInternalId)
        );

        let result = attributed_tool_result(
            &mut tracker,
            "same-internal",
            &text_result("provider-a", "done"),
        )
        .expect("original call remains pending");
        assert_eq!(result.0, "original");
    }

    #[test]
    fn runner_pending_tool_call_limit_fails_closed_at_the_boundary() {
        let mut tracker = ToolCallTracker::default();
        for index in 0..MAX_PENDING_TOOL_CALLS {
            tracker
                .record(
                    &format!("internal-{index}"),
                    &tool_call(&format!("provider-{index}"), "read"),
                )
                .expect("calls through the documented bound must be tracked");
        }

        assert_eq!(
            tracker.record("overflow", &tool_call("provider-overflow", "read")),
            Err(ToolCallTrackerError::CapacityExceeded)
        );
        assert_eq!(tracker.pending.len(), MAX_PENDING_TOOL_CALLS);
    }

    #[test]
    fn resolved_tool_call_churn_keeps_pending_order_bounded() {
        let mut tracker = ToolCallTracker::default();
        for index in 0..(MAX_PENDING_TOOL_CALLS * 4) {
            let internal_id = format!("internal-{index}");
            let provider_id = format!("provider-{index}");
            tracker
                .record(&internal_id, &tool_call(&provider_id, "read"))
                .unwrap();
            attributed_tool_result(
                &mut tracker,
                &internal_id,
                &text_result(&provider_id, "done"),
            )
            .expect("each sequential result resolves its exact call");
            assert!(tracker.pending.is_empty());
            assert!(tracker.order.is_empty());
        }
    }

    #[test]
    fn runner_pending_tool_calls_are_finalized_at_stream_termination() {
        let mut tracker = ToolCallTracker::default();
        let mut interactions = Vec::new();
        tracker
            .record("pending-internal", &tool_call("pending-provider", "read"))
            .unwrap();

        let finalized = tracker.finalize_unresolved(&mut interactions);

        assert!(tracker.pending.is_empty());
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].internal_call_id, "pending-internal");
        assert_eq!(finalized[0].name, "read");
        assert_eq!(finalized[0].tool_result.id, "pending-provider");
        assert_eq!(interactions.len(), 1);
        assert!(
            attributed_tool_result(
                &mut tracker,
                "pending-internal",
                &text_result("pending-provider", "late"),
            )
            .is_none(),
            "a late result must not attach to a correlation from a terminated stream"
        );
    }

    #[test]
    fn unresolved_tool_calls_finalize_in_original_call_order_with_provider_metadata() {
        let mut tracker = ToolCallTracker::default();
        let mut first = tool_call("provider-a", "first");
        first.call_id = Some("provider-call-a".to_string());
        tracker.record("internal-a", &first).unwrap();
        tracker
            .record("internal-b", &tool_call("provider-b", "second"))
            .unwrap();
        tracker
            .record("internal-c", &tool_call("provider-c", "third"))
            .unwrap();
        attributed_tool_result(
            &mut tracker,
            "internal-b",
            &text_result("provider-b", "finished"),
        )
        .expect("the middle call resolves normally");
        let mut interactions = Vec::new();

        let finalized = tracker.finalize_unresolved(&mut interactions);

        assert_eq!(
            finalized
                .iter()
                .map(|call| (
                    call.internal_call_id.as_str(),
                    call.name.as_str(),
                    call.tool_result.id.as_str(),
                    call.tool_result.call_id.as_deref(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("internal-a", "first", "provider-a", Some("provider-call-a")),
                ("internal-c", "third", "provider-c", None),
            ]
        );
        assert_eq!(interactions.len(), 2);
    }

    fn usage(
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        cache_creation_input_tokens: u64,
    ) -> Usage {
        Usage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            ..Usage::new()
        }
    }

    #[test]
    fn final_aggregate_reconciliation_charges_each_usage_field_exactly_once() {
        let first = usage(10, 2, 7, 3);
        let second = usage(20, 4, 5, 1);
        let aggregate = first + second;
        let mut ledger = UsageLedger::default();
        ledger.start_stream();

        assert_eq!(Usage::from(ledger.record(first)), first);
        assert_eq!(Usage::from(ledger.record(second)), second);
        assert_eq!(
            Usage::from(ledger.reconcile_terminal(aggregate)),
            Usage::new(),
            "the terminal aggregate is reconciliation data, not a second charge"
        );
        assert_eq!(ledger.total, aggregate);
    }

    #[test]
    fn final_only_usage_and_regressing_fields_reconcile_without_underflow() {
        let aggregate = usage(30, 6, 9, 2);
        let mut final_only = UsageLedger::default();
        final_only.start_stream();
        assert_eq!(
            Usage::from(final_only.reconcile_terminal(aggregate)),
            aggregate,
            "an adapter with no completion-call event must still be charged once"
        );

        let observed = usage(20, 8, 5, 4);
        let regressed = usage(15, 10, 7, 1);
        let mut mixed = UsageLedger::default();
        mixed.start_stream();
        mixed.record(observed);
        let delta = Usage::from(mixed.reconcile_terminal(regressed));
        assert_eq!(delta.input_tokens, 0);
        assert_eq!(delta.output_tokens, 2);
        assert_eq!(delta.cached_input_tokens, 2);
        assert_eq!(delta.cache_creation_input_tokens, 0);
        assert_eq!(
            super::exhausted_token_budget(final_only.total, Some(36)),
            Some((36, 36))
        );
    }

    #[test]
    fn usage_reconciliation_resets_its_aggregate_scope_for_continuation_streams() {
        let first = usage(10, 2, 3, 1);
        let second = usage(20, 4, 5, 2);
        let mut ledger = UsageLedger::default();
        ledger.start_stream();
        ledger.record(first);
        assert!(!ledger.reconcile_terminal(first).has_values());

        ledger.start_stream();
        ledger.record(second);
        assert!(!ledger.reconcile_terminal(second).has_values());
        assert_eq!(ledger.total, first + second);
    }

    #[test]
    fn usage_reconciliation_is_exact_once_across_stream_saturation() {
        let prior = Usage {
            input_tokens: 10,
            output_tokens: 10,
            total_tokens: 10,
            cached_input_tokens: 10,
            cache_creation_input_tokens: 10,
            tool_use_prompt_tokens: 10,
            reasoning_tokens: 10,
        };
        let saturated = Usage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            total_tokens: u64::MAX,
            cached_input_tokens: u64::MAX,
            cache_creation_input_tokens: u64::MAX,
            tool_use_prompt_tokens: u64::MAX,
            reasoning_tokens: u64::MAX,
        };
        let mut ledger = UsageLedger::default();

        ledger.start_stream();
        ledger.record(prior);
        assert!(!ledger.reconcile_terminal(prior).has_values());

        ledger.start_stream();
        assert_eq!(Usage::from(ledger.record(saturated)), saturated);
        assert!(ledger.stream_has_observed_usage());
        assert_eq!(
            Usage::from(ledger.reconcile_terminal(saturated)),
            Usage::new(),
            "saturation must not turn the prior stream's usage into a duplicate terminal charge"
        );
        assert_eq!(ledger.total, saturated);
    }

    #[test]
    fn usage_ledger_saturates_every_field_and_budget_observation_at_u64_max() {
        let near_max = Usage {
            input_tokens: u64::MAX - 1,
            output_tokens: u64::MAX - 1,
            total_tokens: u64::MAX - 1,
            cached_input_tokens: u64::MAX - 1,
            cache_creation_input_tokens: u64::MAX - 1,
            tool_use_prompt_tokens: u64::MAX - 1,
            reasoning_tokens: u64::MAX - 1,
        };
        let increment = Usage {
            input_tokens: 10,
            output_tokens: 10,
            total_tokens: 10,
            cached_input_tokens: 10,
            cache_creation_input_tokens: 10,
            tool_use_prompt_tokens: 10,
            reasoning_tokens: 10,
        };
        let saturated = Usage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            total_tokens: u64::MAX,
            cached_input_tokens: u64::MAX,
            cache_creation_input_tokens: u64::MAX,
            tool_use_prompt_tokens: u64::MAX,
            reasoning_tokens: u64::MAX,
        };
        let mut ledger = UsageLedger::default();
        ledger.start_stream();

        ledger.record(near_max);
        ledger.record(increment);

        assert_eq!(ledger.total, saturated);
        assert_eq!(
            super::exhausted_token_budget(ledger.total, Some(u64::MAX - 1)),
            Some((u64::MAX, u64::MAX - 1))
        );
    }

    #[tokio::test]
    async fn rig_multi_turn_run_usage_saturates_before_completion_delivery() {
        let near_max = Usage {
            input_tokens: u64::MAX - 1,
            output_tokens: u64::MAX - 1,
            total_tokens: u64::MAX - 1,
            cached_input_tokens: u64::MAX - 1,
            cache_creation_input_tokens: u64::MAX - 1,
            tool_use_prompt_tokens: u64::MAX - 1,
            reasoning_tokens: u64::MAX - 1,
        };
        let increment = Usage {
            input_tokens: 10,
            output_tokens: 10,
            total_tokens: 10,
            cached_input_tokens: 10,
            cache_creation_input_tokens: 10,
            tool_use_prompt_tokens: 10,
            reasoning_tokens: 10,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "saturating-run-tool",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response(near_max),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response(increment),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .tool(CountingTool(calls.clone()))
            .default_max_turns(2)
            .build();

        let (response, usage) = super::run_print(
            &agent,
            "start",
            false,
            &crate::retry::RetryConfig::default(),
            Vec::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .expect("Rig run-level aggregation must saturate rather than panic or wrap");

        assert_eq!(response, "done");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(model.requests().len(), 2);
        assert_eq!(usage.input_tokens, u64::MAX);
        assert_eq!(usage.output_tokens, u64::MAX);
        assert_eq!(usage.total_tokens, u64::MAX);
        assert_eq!(usage.cached_input_tokens, u64::MAX);
        assert_eq!(usage.cache_creation_input_tokens, u64::MAX);
        assert_eq!(usage.tool_use_prompt_tokens, u64::MAX);
        assert_eq!(usage.reasoning_tokens, u64::MAX);
    }

    #[tokio::test]
    async fn over_budget_tool_completion_stops_before_next_provider_call_on_both_surfaces() {
        let over_budget = usage(40, 15, 0, 0);
        let interactive_calls = Arc::new(AtomicUsize::new(0));
        let interactive_model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "over-budget-tool",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response(over_budget),
            ],
            vec![
                MockStreamEvent::text("must not run"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let interactive_agent = AgentBuilder::new(interactive_model.clone())
            .tool(CountingTool(interactive_calls.clone()))
            .max_tokens(50)
            .default_max_turns(2)
            .build();
        let mut runner = super::spawn_agent(
            interactive_agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let interactive_error = loop {
            match runner.event_rx.recv().await {
                Some(crate::event::AgentEvent::Error(error)) => break error.to_string(),
                Some(crate::event::AgentEvent::Done { response, .. }) => {
                    panic!("over-budget tool completion unexpectedly finished: {response}")
                }
                Some(_) => {}
                None => panic!("interactive runner ended without a budget diagnostic"),
            }
        };

        let headless_calls = Arc::new(AtomicUsize::new(0));
        let headless_model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "over-budget-tool",
                    CountingTool::NAME,
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response(over_budget),
            ],
            vec![
                MockStreamEvent::text("must not run"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let headless_agent = AgentBuilder::new(headless_model.clone())
            .tool(CountingTool(headless_calls.clone()))
            .max_tokens(50)
            .default_max_turns(2)
            .build();
        let headless_error = super::run_print(
            &headless_agent,
            "start",
            false,
            &crate::retry::RetryConfig::default(),
            Vec::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .expect_err("headless over-budget tool completion must fail before another provider call");

        assert!(interactive_error.contains("55/50"));
        assert_eq!(headless_error.to_string(), interactive_error);
        assert_eq!(interactive_calls.load(Ordering::SeqCst), 0);
        assert_eq!(headless_calls.load(Ordering::SeqCst), 0);
        assert_eq!(interactive_model.requests().len(), 1);
        assert_eq!(headless_model.requests().len(), 1);
    }

    #[tokio::test]
    async fn over_budget_text_completion_preserves_terminal_response() {
        let over_budget = usage(40, 15, 0, 0);
        let interactive_model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response(over_budget),
        ]]);
        let interactive_agent = AgentBuilder::new(interactive_model).max_tokens(50).build();
        let mut runner = super::spawn_agent(
            interactive_agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let interactive_response = loop {
            match runner.event_rx.recv().await {
                Some(crate::event::AgentEvent::Done { response, .. }) => {
                    break response.to_string();
                }
                Some(crate::event::AgentEvent::Error(error)) => {
                    panic!("terminal text response must be preserved: {error}")
                }
                Some(_) => {}
                None => panic!("interactive runner ended without a terminal response"),
            }
        };

        let headless_model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response(over_budget),
        ]]);
        let headless_agent = AgentBuilder::new(headless_model).max_tokens(50).build();
        let (headless_response, headless_usage) = super::run_print(
            &headless_agent,
            "start",
            false,
            &crate::retry::RetryConfig::default(),
            Vec::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .expect("headless terminal text response must be preserved");

        assert_eq!(interactive_response, "done");
        assert_eq!(headless_response, "done");
        assert_eq!(headless_usage, over_budget);
    }

    #[tokio::test]
    async fn two_call_tool_continuation_emits_one_chargeable_delta_model_on_both_surfaces() {
        let first = usage(10, 2, 7, 3);
        let second = usage(20, 4, 5, 1);
        let aggregate = first + second;
        let interactive_model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("usage-call", CountingTool::NAME, serde_json::json!({})),
                MockStreamEvent::final_response(first),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response(second),
            ],
        ]);
        let interactive_agent = AgentBuilder::new(interactive_model)
            .tool(CountingTool(Arc::new(AtomicUsize::new(0))))
            .default_max_turns(2)
            .build();
        let mut runner = super::spawn_agent(
            interactive_agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );

        let mut interactive_usage = Usage::new();
        let mut delta_count = 0;
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::UsageDelta {
                    usage: delta,
                    context_complete,
                } => {
                    assert!(
                        context_complete,
                        "completion-call deltas are complete snapshots"
                    );
                    interactive_usage += Usage::from(delta);
                    delta_count += 1;
                }
                crate::event::AgentEvent::Done { response, .. } => {
                    assert_eq!(response, "done");
                    break;
                }
                crate::event::AgentEvent::Error(error) => panic!("unexpected error: {error}"),
                _ => {}
            }
        }
        assert_eq!(interactive_usage, aggregate);
        assert_eq!(
            delta_count, 2,
            "terminal aggregate must not emit a duplicate delta"
        );

        let headless_model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("usage-call", CountingTool::NAME, serde_json::json!({})),
                MockStreamEvent::final_response(first),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response(second),
            ],
        ]);
        let headless_agent = AgentBuilder::new(headless_model)
            .tool(CountingTool(Arc::new(AtomicUsize::new(0))))
            .default_max_turns(2)
            .build();
        let (_, headless_usage) = super::run_print(
            &headless_agent,
            "start",
            false,
            &crate::retry::RetryConfig::default(),
            Vec::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .expect("headless continuation must complete");
        assert_eq!(headless_usage, aggregate);
    }

    #[tokio::test]
    async fn final_only_adapter_usage_is_charged_once_on_both_surfaces() {
        let aggregate = usage(30, 6, 9, 2);
        let interactive_model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response(aggregate),
        ]]);
        let interactive_agent = AgentBuilder::new(interactive_model).build();
        let mut runner = super::spawn_agent_with_stream_policy(
            interactive_agent,
            "start".to_string(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            RunnerStreamPolicy::without_completion_calls(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let mut deltas = Vec::new();
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::UsageDelta {
                    usage: delta,
                    context_complete,
                } => {
                    assert!(
                        context_complete,
                        "final-only usage is a complete fallback snapshot"
                    );
                    deltas.push(Usage::from(delta));
                }
                crate::event::AgentEvent::Done { response, .. } => {
                    assert_eq!(response, "done");
                    break;
                }
                crate::event::AgentEvent::Error(error) => panic!("unexpected error: {error}"),
                _ => {}
            }
        }
        assert_eq!(deltas, [aggregate]);

        let headless_model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response(aggregate),
        ]]);
        let headless_agent = AgentBuilder::new(headless_model).build();
        let (_, headless_usage) = super::run_print_with_stream_policy(
            &headless_agent,
            "start",
            false,
            &crate::retry::RetryConfig::default(),
            Vec::new(),
            RunnerStreamPolicy::without_completion_calls(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .expect("final-only headless adapter must complete");
        assert_eq!(headless_usage, aggregate);
    }

    #[test]
    fn streamed_reasoning_delta_is_forwardable_as_reasoning_text() {
        let content = StreamedAssistantContent::<()>::ReasoningDelta {
            id: Some("rs_demo".to_string()),
            reasoning: "thinking in progress".to_string(),
        };

        assert_eq!(
            streamed_reasoning_text(&content).as_deref(),
            Some("thinking in progress")
        );
    }

    #[test]
    fn empty_reasoning_delta_is_ignored() {
        let content = StreamedAssistantContent::<()>::ReasoningDelta {
            id: None,
            reasoning: String::new(),
        };

        assert!(streamed_reasoning_text(&content).is_none());
    }
}
