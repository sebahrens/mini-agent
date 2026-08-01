use compact_str::CompactString;
use futures::StreamExt;
use rig::agent::{Agent, MultiTurnStreamItem, StreamingResult};
use rig::completion::Usage;
#[cfg(feature = "multimodal")]
use rig::completion::message::{AudioMediaType, DocumentMediaType, ImageMediaType};
use rig::completion::{CompletionModel, Message};
use rig::message::{AssistantContent, ToolResult, ToolResultContent};
use rig::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingChat};
use tokio::sync::mpsc;

use crate::event::{AgentEvent, BtwEvent};
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

/// Handle to an in-flight `/btw` side-question task. The `abort_handle` lets the
/// UI cancel the side question (e.g. on Ctrl-C) without touching the main agent.
pub struct BtwRunner {
    pub abort_handle: tokio::task::AbortHandle,
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

fn attributed_tool_result(
    last_tool_name: &mut Option<String>,
    tool_result: &ToolResult,
) -> Option<(CompactString, String)> {
    let Some(tool_name) = last_tool_name.take() else {
        tracing::error!(
            tool_result_id = %tool_result.id,
            "agent received tool result without a preceding tool call; skipping"
        );
        return None;
    };

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
            tool_result_id = %tool_result.id,
            non_text_content_count,
            "agent tool result contained no text content; using a visible fallback"
        );
        output
            .push_str("[Tool result contained non-text content that cannot be displayed as text.]");
    }

    Some((CompactString::new(tool_name), output))
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

    if let Some(Message::Assistant { content, .. }) = interactions.last_mut()
        && let AssistantContent::Text(previous) = content.last_mut()
        && previous.additional_params.is_none()
    {
        previous.text.push_str(text);
        return;
    }

    interactions.push(Message::assistant(text.to_string()));
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
}

impl RunnerStreamPolicy {
    #[cfg(test)]
    fn drop_next_terminal_responses(count: usize) -> Self {
        Self {
            drop_terminal_responses: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                count,
            )),
        }
    }

    fn apply<R>(&self, stream: StreamingResult<R>) -> StreamingResult<R>
    where
        R: Send + 'static,
    {
        #[cfg(test)]
        if self
            .drop_terminal_responses
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| (remaining > 0).then(|| remaining - 1),
            )
            .is_ok()
        {
            return stream
                .filter(|item| {
                    std::future::ready(!matches!(item, Ok(MultiTurnStreamItem::FinalResponse(_))))
                })
                .boxed();
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
    let join = tokio::spawn(async move {
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
    });

    BtwRunner {
        abort_handle: join.abort_handle(),
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
    retry_prompt: &str,
    retry_history: &[Message],
    new_tool_interactions: &[Message],
    retry_config: &RetryConfig,
    max_turns: usize,
) -> StreamingResult<M::StreamingResponse>
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let mut new_history = retry_history.to_vec();
    new_history.extend_from_slice(new_tool_interactions);
    new_history.push(Message::user(retry_prompt.to_string()));
    new_history.push(Message::assistant(String::new()));
    match retry::retry_stream_chat(retry_config, || {
        let h = new_history.clone();
        async move {
            agent
                .stream_chat("Please continue.", h)
                .max_turns(max_turns)
                .await
        }
    })
    .await
    {
        Ok(stream) => stream,
        Err(e) => Box::pin(futures::stream::once(async move { Err(e) })),
    }
}

fn take_new_tool_interactions(tool_interactions: &mut Vec<Message>) -> Vec<Message> {
    let new_tool_interactions = std::mem::take(tool_interactions);
    tracing::debug!(
        "agent injecting continue prompt, new_tool_interactions={}",
        new_tool_interactions.len(),
    );
    new_tool_interactions
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
    spawn_agent_with_stream_policy(
        agent,
        prompt,
        history,
        retry_config,
        RunnerStreamPolicy::default(),
        #[cfg(feature = "skills")]
        turn_guard,
        #[cfg(feature = "hooks")]
        loop_info,
    )
}

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
    let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(32);

    #[cfg(feature = "subagents")]
    let subagent_event_tx = event_tx.clone();

    let agent_future = async move {
        #[cfg(feature = "skills")]
        let _turn_guard = turn_guard;
        tracing::debug!(
            "spawn_agent: prompt_len={}, history_len={}, max_attempts={}",
            prompt.len(),
            history.len(),
            retry_config.max_attempts,
        );
        let retry_prompt = prompt.clone();
        let retry_history: Vec<Message> = history.clone();
        let mut tool_interactions: Vec<Message> = Vec::new();
        let mut last_tool_name: Option<String> = None;
        let mut empty_response_count: u32 = 0;
        const MAX_EMPTY_RESPONSES: u32 = 3;
        let max_turns = agent.default_max_turns.unwrap_or(1);
        let mut turns_used = 0usize;
        let mut turns_at_stream_start = turns_used;
        let mut response = String::new();
        let mut response_len_at_stream_start = response.len();
        let mut cumulative_usage = Usage::new();
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
                    .stream_chat(prompt.clone(), history.clone())
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
                                append_streamed_text(&mut tool_interactions, &text.text);
                                let _ = event_tx
                                    .send(AgentEvent::Token(CompactString::from(text.text)))
                                    .await;
                            }
                            StreamedAssistantContent::ToolCall { tool_call, .. } => {
                                let tool_name = &tool_call.function.name;
                                tracing::debug!(
                                    "agent tool start: name={}, args_len={}",
                                    tool_name,
                                    tool_call.function.arguments.to_string().len(),
                                );
                                last_tool_name = Some(tool_name.clone());
                                tool_interactions.push(tool_call.clone().into());
                                let _ = event_tx
                                    .send(AgentEvent::ToolCall {
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
                        tracing::debug!(
                            tool_name = %tool_call.function.name,
                            internal_call_id,
                            "agent tool execution started"
                        );
                    }
                    Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                        tool_result,
                        ..
                    })) => {
                        let Some((tool_name, output)) =
                            attributed_tool_result(&mut last_tool_name, &tool_result)
                        else {
                            continue;
                        };
                        tracing::debug!(
                            "agent tool result: name={}, output_len={}",
                            tool_name,
                            output.len(),
                        );
                        let _ = event_tx
                            .send(AgentEvent::ToolResult {
                                name: tool_name.clone(),
                                output: CompactString::from(output),
                            })
                            .await;
                        tool_interactions.push(tool_result.clone().into());
                    }
                    Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                        terminal_response_seen = true;
                        let usage = res.usage();
                        let response_text = res.output;
                        reconcile_terminal_response(
                            &mut response,
                            response_len_at_stream_start,
                            &response_text,
                        );
                        tracing::info!(
                            "agent done: input_tokens={}, output_tokens={}, cached_input_tokens={}, cache_creation_input_tokens={}",
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.cached_input_tokens,
                            usage.cache_creation_input_tokens,
                        );

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
                                    input_tokens: usage.input_tokens,
                                    output_tokens: usage.output_tokens,
                                    cached_input_tokens: usage.cached_input_tokens,
                                    cache_creation_input_tokens: usage.cache_creation_input_tokens,
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
                        cumulative_usage += usage;
                        tracing::debug!(
                            "agent completion: input_tokens={}, output_tokens={}, cumulative_tokens={}",
                            usage.input_tokens,
                            usage.output_tokens,
                            observed_tokens(cumulative_usage),
                        );
                        let _ = event_tx
                            .send(AgentEvent::CompletionCall {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                cached_input_tokens: usage.cached_input_tokens,
                                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                            })
                            .await;
                    }
                    Err(e) => {
                        tracing::error!("agent stream error: {e}");
                        let _ = event_tx
                            .send(AgentEvent::Error(CompactString::new(e.to_string())))
                            .await;
                        return;
                    }
                    Ok(item) => warn_unknown_stream_item(&item),
                }
            }

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
            if let Some((used, budget)) = exhausted_token_budget(cumulative_usage, agent.max_tokens)
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
            let injected_prompt = next_instruction
                .take()
                .unwrap_or_else(|| retry_prompt.clone());
            let new_tool_interactions = take_new_tool_interactions(&mut tool_interactions);
            stream = stream_policy.apply(
                continue_prompt_injector(
                    &agent,
                    &injected_prompt,
                    &retry_history,
                    &new_tool_interactions,
                    &retry_config,
                    remaining_turns,
                )
                .await,
            );
            turns_at_stream_start = turns_used;
            response_len_at_stream_start = response.len();
        }
    };

    #[cfg(feature = "subagents")]
    let agent_future =
        crate::extras::subagents::scope_subagent_event_tx(subagent_event_tx, agent_future);

    let join = tokio::spawn(agent_future);

    AgentRunner {
        event_rx,
        abort_handle: join.abort_handle(),
    }
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
    let mut tool_interactions: Vec<Message> = Vec::new();
    let mut full_response = String::new();
    let mut response_len_at_stream_start = full_response.len();
    let mut last_tool_name: Option<String> = None;
    let mut usage = rig::completion::Usage::new();
    let mut cumulative_usage = Usage::new();
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
                    append_streamed_text(&mut tool_interactions, &text.text);
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
                    StreamedAssistantContent::ToolCall { tool_call, .. },
                )) => {
                    let name = &tool_call.function.name;
                    last_tool_name = Some(name.clone());
                    if pure_stdout {
                        let summary = format_tool_args_summary(&tool_call.function.arguments);
                        println!("\n◈ {} {}", name, summary);
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    tool_interactions.push(tool_call.clone().into());
                }
                Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    ..
                })) => {
                    let Some((name, output)) =
                        attributed_tool_result(&mut last_tool_name, &tool_result)
                    else {
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
                    tool_interactions.push(tool_result.clone().into());
                }
                Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                    turns_used = turns_used.saturating_add(1);
                    cumulative_usage += call.usage;
                    tracing::debug!(
                        "agent completion: input_tokens={}, output_tokens={}, cumulative_tokens={}",
                        call.usage.input_tokens,
                        call.usage.output_tokens,
                        observed_tokens(cumulative_usage),
                    );
                }
                Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                    terminal_response_seen = true;
                    usage = res.usage();
                    reconcile_terminal_response(
                        &mut full_response,
                        response_len_at_stream_start,
                        &res.output,
                    );
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
                    return Err(anyhow::anyhow!("{e}"));
                }
            }
        }

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
            if let Some((used, budget)) = exhausted_token_budget(cumulative_usage, agent.max_tokens)
            {
                anyhow::bail!(
                    "Agent exhausted its cumulative token budget ({used}/{budget}) before \
                     completing. Compact the session or increase max_tokens before retrying."
                );
            }
            #[cfg(feature = "hooks")]
            let injected_prompt = next_instruction
                .take()
                .unwrap_or_else(|| prompt.to_string());
            #[cfg(not(feature = "hooks"))]
            let injected_prompt = prompt.to_string();
            let new_tool_interactions = take_new_tool_interactions(&mut tool_interactions);
            // Keep the text already streamed to stdout this turn: the caller
            // persists the returned string as the assistant message, so
            // clearing it here would drop turn-1 output the user already saw
            // and desync the saved transcript from the terminal.
            stream = stream_policy.apply(
                continue_prompt_injector(
                    agent,
                    &injected_prompt,
                    &retry_history,
                    &new_tool_interactions,
                    retry_config,
                    remaining_turns,
                )
                .await,
            );
            turns_at_stream_start = turns_used;
            response_len_at_stream_start = full_response.len();
        }
    }

    println!();
    Ok((full_response, usage))
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
                usage: Usage::new(),
            };
        }
    };

    let mut full_response = String::new();
    let mut usage = Usage::new();

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
                let final_usage = res.usage();
                if final_usage.has_values() {
                    usage = final_usage;
                }
                full_response = res.output.to_string();
                break;
            }
            Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                usage += call.usage;
            }
            Ok(_) => {}
            Err(e) => {
                return SubagentRunOutput {
                    response: Err(format!("subagent error: {e}")),
                    usage,
                };
            }
        }
    }

    if full_response.is_empty() {
        return SubagentRunOutput {
            response: Err("subagent returned empty response".to_string()),
            usage,
        };
    }

    SubagentRunOutput {
        response: Ok(full_response),
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NonTerminalStreamExhausted, RunnerStreamPolicy, attributed_tool_result,
        charge_nonterminal_eof, streamed_reasoning_text, warn_unknown_stream_item,
    };
    use rig::OneOrMany;
    use rig::agent::{AgentBuilder, MultiTurnStreamItem};
    use rig::completion::{Message, Usage};
    use rig::message::{AssistantContent, Image, Text, ToolResult, ToolResultContent};
    use rig::streaming::StreamedAssistantContent;
    use rig::test_utils::{MockCompletionModel, MockStreamEvent, MockToolError};
    use rig::tool::Tool;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    async fn continuations_include_only_tool_interactions_since_the_previous_injection() {
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
            ["tool-second"],
            "the second continuation must not replay interactions sent in the first"
        );
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
        assert!(
            requests[1].chat_history.iter().any(|message| {
                matches!(
                    message,
                    Message::Assistant { content, .. }
                        if content.iter().any(|item| matches!(
                            item,
                            AssistantContent::Text(text) if text.text == "prefix "
                        ))
                )
            }),
            "the continuation request must include the partial assistant prefix"
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
        assert!(tool_call_index < tool_result_index && tool_result_index < partial_text_index);
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
        assert!(model.requests()[1].chat_history.iter().any(|message| {
            matches!(message, Message::Assistant { content, .. } if content.iter().any(
                |item| matches!(item, AssistantContent::Text(text) if text.text == "prefix ")
            ))
        }));
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
        let mut last_tool_name = None;

        assert!(attributed_tool_result(&mut last_tool_name, &tool_result).is_none());
    }

    #[test]
    fn non_text_tool_result_has_visible_fallback_output() {
        let tool_result = ToolResult {
            id: "image-result".to_string(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::Image(Image::default())),
        };
        let mut last_tool_name = Some("image_tool".to_string());

        let (tool_name, output) = attributed_tool_result(&mut last_tool_name, &tool_result)
            .expect("a result with a preceding tool call must be attributed");

        assert_eq!(tool_name, "image_tool");
        assert_eq!(
            output,
            "[Tool result contained non-text content that cannot be displayed as text.]"
        );
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
