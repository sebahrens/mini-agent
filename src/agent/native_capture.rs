//! Bounded, invocation-owned observation of accepted native SDK content.
//!
//! This is compatibility data, never tool/security authority. No Session replay
//! policy is enabled here; the caller must later prove the actual request prefix.

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::session::native_replay::{NativeCommitment, RestartReason};
use rig::OneOrMany;
use rig::agent::{AgentHook, Flow, HookContext, StepEvent, StepEventKind};
use rig::completion::{AssistantContent, CompletionModel, CompletionRequest, Message};

const MAX_TURNS: usize = 128;
const MAX_BLOCKS: usize = 256;
const MAX_TURN_BYTES: usize = 2 * 1024 * 1024;
const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;
const MAX_IDENTITY_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Unavailable {
    Limit,
    RecoveredToolContent,
    MissingCallIdentity,
    InconsistentCompletedTurn,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdvertisedToolIdentity {
    pub(crate) tool_id: String,
    pub(crate) provider_id: Option<String>,
    pub(crate) internal_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTurnProjection {
    pub(crate) sdk_run_id: String,
    pub(crate) turn: usize,
    /// Read from the actual native typed model, not inferred from its name.
    /// Raw request overrides/prefix compatibility are deliberately not proven.
    pub(crate) configured_model: String,
    pub(crate) content: OneOrMany<AssistantContent>,
    pub(crate) advertised_tools: Vec<AdvertisedToolIdentity>,
    /// Exact successfully correlated, post-rewrite typed results in SDK yield
    /// order. Partial sets are data only, never a ready grouped reply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) native_tool_results: Vec<rig::message::ToolResult>,
    /// Actual prepared input delta, excluding only the builder-owned preamble.
    /// None is the old observation-only mode, not replayable provenance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) native_input_delta: Option<Vec<Message>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) native_commitment: Option<NativeCommitment>,
}

#[derive(Default)]
struct State {
    turns: Vec<NativeTurnProjection>,
    keys: HashSet<(String, usize)>,
    bytes: usize,
    /// Set only by an actual canonical ModelTurnFinished callback, before
    /// finite projection limits. Restored frames do not mint new progress.
    accepted_new_turn: bool,
    empty_native_end_turn: bool,
    unavailable: Option<Unavailable>,
    replay: Option<ReplayState>,
    actor_pair: Option<crate::session::native_publication::NativeActorPair>,
    prepared_observed: bool,
    host: Option<crate::provider::NativeHostGuard>,
}

type NativeBodyFactory = Arc<
    dyn Fn(CompletionRequest) -> Result<serde_json::Value, rig::completion::CompletionError>
        + Send
        + Sync,
>;
struct PreparedInput {
    key: (String, usize),
    request: CompletionRequest,
    input: Vec<Message>,
    delta: Vec<Message>,
}
struct ReplayState {
    endpoint: http::Uri,
    body: NativeBodyFactory,
    generated_preamble: bool,
    manual_cache_markers: bool,
    epoch: u64,
    frontier: Option<crate::session::native_replay::NativeFrontier>,
    accepted_history: Vec<Message>,
    initial: Option<Vec<Message>>,
    pending: Option<PreparedInput>,
    commitment: Option<NativeCommitment>,
    failure: Option<RestartReason>,
    payload_limit: usize,
    frame_limit: usize,
}

#[derive(Clone, Default)]
pub(crate) struct NativeCapture(Arc<Mutex<State>>);

impl NativeCapture {
    pub(crate) fn bind_host(
        &self,
        host: Option<crate::provider::NativeHostGuard>,
    ) -> Result<(), RestartReason> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.prepared_observed || state.accepted_new_turn {
            return Err(RestartReason::UnavailableSource);
        }
        state.host = host;
        Ok(())
    }
    pub(crate) fn pair_publication(
        &self,
        pair: crate::session::native_publication::NativeActorPair,
    ) -> Result<(), RestartReason> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.actor_pair.is_some()
            || state.prepared_observed
            || state.accepted_new_turn
            || state
                .replay
                .as_ref()
                .is_none_or(|replay| replay.pending.is_some())
        {
            return Err(RestartReason::UnavailableSource);
        }
        state.actor_pair = Some(pair);
        Ok(())
    }

    pub(crate) fn actor_pair(&self) -> Option<crate::session::native_publication::NativeActorPair> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .actor_pair
            .clone()
    }

    #[allow(
        dead_code,
        reason = "shared API is intentionally dormant before its frontend consumer lands"
    )]
    pub(crate) fn reserve_retired_history(
        &self,
        retired: &[crate::session::native_replay::NativeHistory],
    ) -> Result<(), RestartReason> {
        let bytes = crate::session::native_replay::bounded_serialized(
            &retired,
            crate::session::native_replay::MAX_NATIVE_PAYLOAD,
        )?;
        let frames: usize = retired.iter().map(|history| history.frames().len()).sum();
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let replay = state
            .replay
            .as_mut()
            .ok_or(RestartReason::UnavailableSource)?;
        replay.payload_limit = crate::session::native_replay::MAX_NATIVE_PAYLOAD
            .checked_sub(bytes.saturating_add(256))
            .ok_or(RestartReason::Limit)?;
        replay.frame_limit = MAX_TURNS.checked_sub(frames).ok_or(RestartReason::Limit)?;
        Ok(())
    }

    /// Accepted canonical turns are progress even when they contain only an
    /// empty-display signed block. This never counts incomplete stream deltas.
    #[allow(
        dead_code,
        reason = "shared API is intentionally dormant before its frontend consumer lands"
    )]
    pub(crate) fn accepted_progress(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .accepted_new_turn
    }

    #[allow(
        dead_code,
        reason = "shared API is intentionally dormant before its frontend consumer lands"
    )]
    pub(crate) fn replay_failure(&self) -> Option<RestartReason> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state
            .replay
            .as_ref()
            .and_then(|replay| replay.failure)
            .or(state.unavailable.map(|_| RestartReason::UnavailableSource))
    }

    fn completes_without_visible_text(&self) -> bool {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.empty_native_end_turn
            && state.unavailable.is_none()
            && state.replay.as_ref().is_some_and(|replay| {
                replay.failure.is_none()
                    && replay.frontier.as_ref().is_none_or(|frontier| {
                        frontier.ready() && frontier.current() == replay.epoch
                    })
            })
    }

    /// Construct only from the actual typed native adapter. The factory is
    /// owned by this invocation and never observes headers or sends HTTP.
    #[allow(
        dead_code,
        reason = "shared API is intentionally dormant before its frontend consumer lands"
    )]
    pub(crate) fn for_replay(
        model: rig::providers::anthropic::completion::CompletionModel<
            crate::provider_http::ProviderHttpClient,
        >,
        generated_preamble: bool,
    ) -> Result<Self, RestartReason> {
        let endpoint = model
            .replay_endpoint()
            .map_err(|_| RestartReason::ChangedDomain)?;
        let manual_cache_markers = model.replay_manual_cache_markers();
        let replay = ReplayState {
            endpoint,
            body: Arc::new(move |request| model.replay_request_body(request)),
            generated_preamble,
            manual_cache_markers,
            epoch: 0,
            frontier: None,
            accepted_history: Vec::new(),
            initial: None,
            pending: None,
            commitment: None,
            failure: None,
            payload_limit: crate::session::native_replay::MAX_NATIVE_PAYLOAD,
            frame_limit: MAX_TURNS,
        };
        Ok(Self(Arc::new(Mutex::new(State {
            replay: Some(replay),
            ..Default::default()
        }))))
    }

    #[allow(
        dead_code,
        reason = "shared API is intentionally dormant before its frontend consumer lands"
    )]
    pub(crate) fn for_new_replay(
        model: rig::providers::anthropic::completion::CompletionModel<
            crate::provider_http::ProviderHttpClient,
        >,
        generated_preamble: bool,
        frontier: &crate::session::native_replay::NativeFrontier,
    ) -> Result<Self, RestartReason> {
        let capture = Self::for_replay(model, generated_preamble)?;
        {
            let mut state = capture.0.lock().unwrap_or_else(|error| error.into_inner());
            let replay = state.replay.as_mut().unwrap();
            replay.epoch = frontier.current();
            replay.frontier = Some(frontier.clone());
        }
        Ok(capture)
    }

    #[allow(
        dead_code,
        reason = "shared API is intentionally dormant before its frontend consumer lands"
    )]
    pub(crate) fn persisted_history(
        &self,
    ) -> Result<crate::session::native_replay::NativeHistory, RestartReason> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let replay = state
            .replay
            .as_ref()
            .ok_or(RestartReason::UnavailableSource)?;
        if let Some(failure) = replay.failure {
            return Err(failure);
        }
        if state.unavailable.is_some() {
            return Err(RestartReason::UnavailableSource);
        }
        crate::session::native_replay::NativeHistory::from_capture(
            replay.epoch,
            replay
                .initial
                .clone()
                .ok_or(RestartReason::UnavailableSource)?,
            state.turns.clone(),
        )
    }

    #[allow(
        dead_code,
        reason = "shared API is intentionally dormant before its frontend consumer lands"
    )]
    pub(crate) fn for_restored_replay(
        model: rig::providers::anthropic::completion::CompletionModel<
            crate::provider_http::ProviderHttpClient,
        >,
        generated_preamble: bool,
        history: &crate::session::native_replay::NativeHistory,
        frontier: &crate::session::native_replay::NativeFrontier,
        boundary: crate::session::native_replay::ReplayBoundary,
    ) -> Result<Self, RestartReason> {
        let accepted = history.restore_history(
            crate::session::native_replay::HistoryProtocol::AnthropicMessages,
            frontier,
            boundary,
        )?;
        let capture = Self::for_replay(model, generated_preamble)?;
        let mut state = capture.0.lock().unwrap_or_else(|error| error.into_inner());
        state.turns = history.frames().to_vec();
        state.keys = state
            .turns
            .iter()
            .map(|frame| (frame.sdk_run_id.clone(), frame.turn))
            .collect();
        state.bytes =
            crate::session::native_replay::bounded_serialized(history, MAX_CAPTURE_BYTES)?;
        let replay = state.replay.as_mut().unwrap();
        replay.epoch = history.epoch();
        replay.frontier = Some(frontier.clone());
        replay.initial = Some(history.base().to_vec());
        replay.accepted_history = accepted;
        replay.commitment = Some(history.commitment()?);
        drop(state);
        Ok(capture)
    }

    fn prepare(
        &self,
        run_id: &str,
        turn: usize,
        request: &CompletionRequest,
    ) -> Result<(), RestartReason> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.prepared_observed = true;
        if let Some(host) = &state.host {
            host.validate()?;
        }
        state.empty_native_end_turn = false;
        if state.replay.is_some() && state.unavailable.is_some() {
            return Err(RestartReason::UnavailableSource);
        }
        let turn_count = state.turns.len();
        let Some(replay) = &mut state.replay else {
            return Ok(());
        };
        let result = (|| {
            if turn_count >= replay.frame_limit {
                return Err(RestartReason::Limit);
            }
            if let Some(failure) = replay.failure {
                return Err(failure);
            }
            if replay
                .frontier
                .as_ref()
                .is_some_and(|frontier| !frontier.ready() || frontier.current() != replay.epoch)
            {
                return Err(RestartReason::ChangedPrefix);
            }
            if request.additional_params.as_ref().is_some_and(|params| {
                ["system", "messages", "tools", "model"]
                    .iter()
                    .any(|key| params.get(key).is_some())
            }) {
                return Err(RestartReason::UnsupportedOverride);
            }
            // Bound before cloning, then inspect the exact native composition.
            serde_json::to_writer(&mut CountBytes(0), request).map_err(|_| RestartReason::Limit)?;
            let body =
                (replay.body)(request.clone()).map_err(|_| RestartReason::UnavailableSource)?;
            if let Some(commitment) = &replay.commitment {
                commitment.validate_with_policy(
                    &replay.endpoint,
                    &body,
                    replay.manual_cache_markers,
                )?;
            } else {
                NativeCommitment::require_unbound_input(
                    &replay.endpoint,
                    &body,
                    replay.manual_cache_markers,
                )?;
            }
            let mut input: Vec<Message> = request.chat_history.iter().cloned().collect();
            if replay.generated_preamble {
                if !matches!(input.first(), Some(Message::System { .. })) {
                    return Err(RestartReason::UnavailableSource);
                }
                input.remove(0);
            }
            if !input.starts_with(&replay.accepted_history) {
                return Err(RestartReason::ChangedPrefix);
            }
            let delta = if replay.initial.is_none() && replay.accepted_history.is_empty() {
                Vec::new()
            } else {
                input[replay.accepted_history.len()..].to_vec()
            };
            replay.pending = Some(PreparedInput {
                key: (run_id.to_string(), turn),
                request: request.clone(),
                input,
                delta,
            });
            Ok(())
        })();
        if let Err(reason) = result {
            replay.failure = Some(reason);
        }
        result
    }

    fn replay_failed(&self) -> bool {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state
            .replay
            .as_ref()
            .is_some_and(|replay| replay.failure.is_some() || state.unavailable.is_some())
    }

    fn unavailable(&self, reason: Unavailable) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unavailable
            .get_or_insert(reason);
    }

    #[cfg(test)]
    fn record(
        &self,
        run_id: &str,
        turn: usize,
        model: &str,
        content: &OneOrMany<AssistantContent>,
    ) {
        self.record_completed(run_id, turn, model, content, false);
    }

    fn record_completed(
        &self,
        run_id: &str,
        turn: usize,
        model: &str,
        content: &OneOrMany<AssistantContent>,
        native_end_turn: bool,
    ) {
        // Acceptance and retainability are distinct. A completed provider turn
        // that exceeds bounded storage still retires the older replay chain.
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .accepted_new_turn = true;
        if run_id.len() > MAX_IDENTITY_BYTES
            || model.len() > MAX_IDENTITY_BYTES
            || content.len() > MAX_BLOCKS
        {
            self.unavailable(Unavailable::Limit);
            return;
        }
        let mut count = CountBytes(0);
        if serde_json::to_writer(&mut count, content).is_err() {
            self.unavailable(Unavailable::Limit);
            return;
        }
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.unavailable.is_some() {
            return;
        }
        if state.keys.contains(&(run_id.to_string(), turn)) {
            let consistent = state.turns.iter().any(|frame| {
                frame.sdk_run_id == run_id
                    && frame.turn == turn
                    && frame.configured_model == model
                    && frame.content == *content
            });
            if !consistent {
                state.unavailable = Some(Unavailable::InconsistentCompletedTurn);
            }
            return;
        }
        let captured_bytes = count
            .0
            .saturating_add(run_id.len())
            .saturating_add(model.len());
        if state.turns.len() >= MAX_TURNS
            || state.bytes.saturating_add(captured_bytes) > MAX_CAPTURE_BYTES
        {
            state.unavailable = Some(Unavailable::Limit);
            return;
        }
        state.bytes += captured_bytes;
        state.keys.insert((run_id.to_string(), turn));
        let replay_data = if let Some(replay) = &mut state.replay {
            let captured = (|| {
                let prepared = replay
                    .pending
                    .take()
                    .ok_or(RestartReason::UnavailableSource)?;
                if prepared.key != (run_id.to_string(), turn) {
                    return Err(RestartReason::UnavailableSource);
                }
                let mut completed = prepared.request;
                let assistant = Message::Assistant {
                    id: None,
                    content: content.clone(),
                };
                let mut history: Vec<Message> = completed.chat_history.iter().cloned().collect();
                history.push(assistant.clone());
                completed.chat_history =
                    OneOrMany::many(history).map_err(|_| RestartReason::UnavailableSource)?;
                let body =
                    (replay.body)(completed).map_err(|_| RestartReason::UnavailableSource)?;
                let commitment = NativeCommitment::capture(
                    &replay.endpoint,
                    &body,
                    replay.manual_cache_markers,
                )?;
                let last = body["messages"]
                    .as_array()
                    .ok_or(RestartReason::UnavailableSource)?
                    .len()
                    .checked_sub(1)
                    .ok_or(RestartReason::UnavailableSource)?;
                replay.commitment = Some(commitment.clone());
                if replay.initial.is_none() {
                    replay.initial = Some(prepared.input.clone());
                }
                replay.accepted_history = prepared.input;
                replay.accepted_history.push(assistant);
                Ok((prepared.delta, commitment.only_message(last)))
            })();
            match captured {
                Ok(data) => Some(data),
                Err(reason) => {
                    replay.failure = Some(reason);
                    None
                }
            }
        } else {
            None
        };
        let projection = NativeTurnProjection {
            sdk_run_id: run_id.to_string(),
            turn,
            configured_model: model.to_string(),
            content: content.clone(),
            advertised_tools: Vec::new(),
            native_tool_results: Vec::new(),
            native_input_delta: replay_data.as_ref().map(|(delta, _)| delta.clone()),
            native_commitment: replay_data.map(|(_, commitment)| commitment),
        };
        if let Some(base) = state
            .replay
            .as_ref()
            .and_then(|replay| replay.initial.as_ref())
        {
            let mut candidate = state.turns.clone();
            candidate.push(projection.clone());
            let replay = state.replay.as_ref().unwrap();
            let validated = crate::session::native_replay::NativeHistory::from_capture(
                replay.epoch,
                base.clone(),
                candidate,
            )
            .and_then(|history| {
                if history.frames().len() > replay.frame_limit {
                    return Err(RestartReason::Limit);
                }
                crate::session::native_replay::bounded_serialized(&history, replay.payload_limit)
                    .map(|_| ())
            });
            if let Err(reason) = validated {
                state.replay.as_mut().unwrap().failure.get_or_insert(reason);
            }
        }
        state.turns.push(projection);
        let opaque = content.iter().any(|item| matches!(item,
            AssistantContent::Reasoning(reasoning) if reasoning.content.iter().any(|block| match block {
                rig::message::ReasoningContent::Text {signature:Some(signature),..} => !signature.is_empty(),
                rig::message::ReasoningContent::Redacted {data} | rig::message::ReasoningContent::Encrypted(data) => !data.is_empty(),
                _=>false,
            })));
        let no_visible_text_or_tools = content.iter().all(|item| match item {
            AssistantContent::Reasoning(_) => true,
            AssistantContent::Text(text) => text.text.is_empty(),
            _ => false,
        });
        state.empty_native_end_turn = native_end_turn && opaque && no_visible_text_or_tools;
    }

    fn tool(
        &self,
        run_id: &str,
        turn: usize,
        tool_id: &str,
        provider_id: Option<&str>,
        internal_id: &str,
    ) {
        if tool_id.is_empty()
            || tool_id.len() > MAX_IDENTITY_BYTES
            || internal_id.is_empty()
            || internal_id.len() > MAX_IDENTITY_BYTES
            || provider_id.is_some_and(|id| id.len() > MAX_IDENTITY_BYTES)
        {
            self.unavailable(Unavailable::MissingCallIdentity);
            return;
        }
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.unavailable.is_some() {
            return;
        }
        let Some(frame) = state
            .turns
            .iter_mut()
            .find(|frame| frame.sdk_run_id == run_id && frame.turn == turn)
        else {
            state.unavailable = Some(Unavailable::MissingCallIdentity);
            return;
        };
        if frame
            .content
            .iter()
            .filter(|item| matches!(item, AssistantContent::ToolCall(call) if call.id == tool_id))
            .count()
            != 1
        {
            state.unavailable = Some(Unavailable::MissingCallIdentity);
            return;
        }
        if let Some(existing) = frame
            .advertised_tools
            .iter()
            .find(|tool| tool.internal_id == internal_id || tool.tool_id == tool_id)
        {
            if existing.tool_id != tool_id
                || existing.internal_id != internal_id
                || existing.provider_id.as_deref() != provider_id
            {
                state.unavailable = Some(Unavailable::MissingCallIdentity);
            }
            return;
        }
        let identity_bytes = tool_id
            .len()
            .saturating_add(internal_id.len())
            .saturating_add(provider_id.map_or(0, str::len));
        if frame.advertised_tools.len() >= MAX_BLOCKS
            || state.bytes.saturating_add(identity_bytes) > MAX_CAPTURE_BYTES
        {
            state.unavailable = Some(Unavailable::Limit);
            return;
        }
        // Re-borrow after checking the aggregate capture bound.
        state.bytes += identity_bytes;
        let frame = state
            .turns
            .iter_mut()
            .find(|frame| frame.sdk_run_id == run_id && frame.turn == turn)
            .expect("frame already validated");
        frame.advertised_tools.push(AdvertisedToolIdentity {
            tool_id: tool_id.to_string(),
            provider_id: provider_id.map(str::to_string),
            internal_id: internal_id.to_string(),
        });
        // Canonical identity metadata also consumes the captured payload.
        // Refuse the current callback before its effect if it cannot be kept.
        if let Some(replay) = &state.replay {
            let budget = crate::session::native_replay::NativeHistory::from_capture(
                replay.epoch,
                replay.initial.clone().unwrap_or_default(),
                state.turns.clone(),
            )
            .and_then(|history| {
                crate::session::native_replay::bounded_serialized(&history, replay.payload_limit)
                    .map(|_| ())
            });
            if let Err(reason) = budget {
                state.replay.as_mut().unwrap().failure.get_or_insert(reason);
            }
        }
    }

    fn tool_result(&self, internal_id: &str, result: &rig::message::ToolResult) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.unavailable.is_some() {
            return;
        }
        let mut exact = state.turns.iter().enumerate().filter_map(|(index, frame)| {
            frame
                .advertised_tools
                .iter()
                .find(|identity| identity.internal_id == internal_id)
                .map(|identity| {
                    (
                        index,
                        identity.tool_id.clone(),
                        identity.provider_id.clone(),
                    )
                })
        });
        let Some((index, canonical, provider)) = exact.next() else {
            state.unavailable = Some(Unavailable::MissingCallIdentity);
            return;
        };
        if exact.next().is_some()
            || index + 1 != state.turns.len()
            || result.id != canonical
            || result.call_id != provider
            || state.turns[index]
                .native_tool_results
                .iter()
                .any(|old| old.id == result.id)
        {
            state.unavailable = Some(Unavailable::MissingCallIdentity);
            return;
        }
        let Ok(result_bytes) =
            crate::session::native_replay::bounded_serialized(result, MAX_TURN_BYTES)
        else {
            state.unavailable = Some(Unavailable::Limit);
            return;
        };
        if state.bytes.saturating_add(result_bytes) > MAX_CAPTURE_BYTES {
            state.unavailable = Some(Unavailable::Limit);
            return;
        }
        state.bytes += result_bytes;
        // Exact membership is established above. The SDK surfaces and commits
        // results in the same prepared-call slot order, after all result hooks.
        state.turns[index].native_tool_results.push(result.clone());
        let frame = &state.turns[index];
        if crate::session::native_replay::bounded_serialized(frame, MAX_TURN_BYTES).is_err() {
            state.unavailable = Some(Unavailable::Limit);
            return;
        }
        let calls = frame
            .content
            .iter()
            .filter(|content| matches!(content, AssistantContent::ToolCall(_)))
            .count();
        let complete =
            frame.native_tool_results.len() == calls && frame.advertised_tools.len() == calls;
        let grouped = complete.then(|| Message::User {
            content: OneOrMany::many(
                frame
                    .native_tool_results
                    .iter()
                    .cloned()
                    .map(rig::message::UserContent::ToolResult),
            )
            .expect("complete nonempty tool batch"),
        });
        if let Some(replay) = &state.replay {
            let validated = crate::session::native_replay::NativeHistory::from_capture(
                replay.epoch,
                replay.initial.clone().unwrap_or_default(),
                state.turns.clone(),
            )
            .and_then(|history| {
                crate::session::native_replay::bounded_serialized(&history, replay.payload_limit)
                    .map(|_| ())
            });
            if let Err(reason) = validated {
                state.replay.as_mut().unwrap().failure.get_or_insert(reason);
                return;
            }
        }
        if let Some(grouped) = grouped
            && let Some(replay) = &mut state.replay
        {
            // Do not reconstruct pending data from a later request or a name.
            // One canonical batch becomes one User, exactly as AgentRun does.
            replay.accepted_history.push(grouped);
        }
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> (Vec<NativeTurnProjection>, Option<Unavailable>) {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        (state.turns.clone(), state.unavailable)
    }
}

struct CountBytes(usize);
impl Write for CountBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_TURN_BYTES)
            .ok_or_else(|| std::io::Error::other("native projection exceeds limits"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
pub(crate) struct NativeCaptureHook {
    native: bool,
    capture: Option<(NativeCapture, String)>,
}

impl NativeCaptureHook {
    pub(crate) fn actor_pair(&self) -> Option<crate::session::native_publication::NativeActorPair> {
        if !self.native {
            return None;
        }
        self.capture
            .as_ref()
            .and_then(|(capture, _)| capture.actor_pair())
    }
    pub(crate) fn for_native(capture: Option<NativeCapture>, model: &str) -> Self {
        if model.len() > MAX_IDENTITY_BYTES {
            if let Some(capture) = capture {
                capture.unavailable(Unavailable::Limit);
            }
            return Self {
                native: true,
                capture: None,
            };
        }
        Self {
            native: true,
            capture: capture.map(|capture| (capture, model.to_string())),
        }
    }
    pub(crate) fn tool_result(&self, internal_id: &str, result: &rig::message::ToolResult) {
        if let Some((capture, _)) = &self.capture {
            capture.tool_result(internal_id, result);
        }
    }

    pub(crate) fn completes_without_visible_text(&self) -> bool {
        self.native
            && self
                .capture
                .as_ref()
                .is_some_and(|(capture, _)| capture.completes_without_visible_text())
    }

    pub(crate) fn is_native(&self) -> bool {
        self.native
    }
}

impl<M: CompletionModel> AgentHook<M> for NativeCaptureHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        self.capture.is_some()
            && matches!(
                kind,
                StepEventKind::CompletionRequestPrepared
                    | StepEventKind::ModelTurnFinished
                    | StepEventKind::InvalidToolCall
                    | StepEventKind::ToolCall
            )
    }
    async fn on_event(&self, ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        if let Some((capture, model)) = &self.capture {
            match event {
                StepEvent::CompletionRequestPrepared { request, turn } => {
                    if let Err(reason) = capture.prepare(ctx.run_id().as_str(), turn, request) {
                        return Flow::terminate(reason.to_string());
                    }
                }
                StepEvent::ModelTurnFinished {
                    turn,
                    content,
                    native_end_turn,
                    ..
                } => capture.record_completed(
                    ctx.run_id().as_str(),
                    turn,
                    model,
                    content,
                    native_end_turn,
                ),
                StepEvent::InvalidToolCall(_) => {
                    capture.unavailable(Unavailable::RecoveredToolContent);
                    if capture.replay_failed() {
                        return Flow::terminate("native thinking context cannot be reproduced");
                    }
                }
                StepEvent::ToolCall {
                    tool_id,
                    tool_call_id,
                    internal_call_id,
                    ..
                } => {
                    let host_valid = capture
                        .0
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .host
                        .as_ref()
                        .is_none_or(|host| host.validate().is_ok());
                    if !host_valid {
                        capture.unavailable(Unavailable::InconsistentCompletedTurn);
                    }
                    if capture.replay_failed() {
                        return Flow::skip("native thinking context cannot be reproduced");
                    }
                    capture.tool(
                        ctx.run_id().as_str(),
                        ctx.turn(),
                        tool_id,
                        tool_call_id,
                        internal_call_id,
                    );
                    if capture.replay_failed() {
                        return Flow::skip("native thinking context cannot be reproduced");
                    }
                }
                _ => {}
            }
        }
        // Never suppress canonical accounting/flush or modify effect Flow.
        Flow::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_capture_dedupes_exact_sdk_request_and_bounds_without_truncation() {
        let capture = NativeCapture::default();
        let content = OneOrMany::one(AssistantContent::text("original"));
        capture.record("one", 1, "native-model", &content);
        capture.record("one", 1, "native-model", &content);
        assert_eq!(capture.snapshot().0.len(), 1);
        capture.record(
            "two",
            1,
            "native-model",
            &OneOrMany::one(AssistantContent::text("x".repeat(MAX_TURN_BYTES + 1))),
        );
        let (turns, unavailable) = capture.snapshot();
        assert_eq!(turns.len(), 1);
        assert_eq!(unavailable, Some(Unavailable::Limit));
        assert_eq!(turns[0].content, content);
    }

    #[test]
    fn native_capture_repeated_key_with_different_content_is_not_silently_reused() {
        let capture = NativeCapture::default();
        capture.record(
            "one",
            1,
            "native-model",
            &OneOrMany::one(AssistantContent::text("original")),
        );
        capture.record(
            "one",
            1,
            "native-model",
            &OneOrMany::one(AssistantContent::text("different")),
        );
        let (frames, unavailable) = capture.snapshot();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].content,
            OneOrMany::one(AssistantContent::text("original"))
        );
        assert_eq!(unavailable, Some(Unavailable::InconsistentCompletedTurn));
    }

    #[test]
    fn native_capture_canonical_tool_id_is_distinct_from_optional_provider_id_and_bijective() {
        let capture = NativeCapture::default();
        let content = OneOrMany::many([
            AssistantContent::tool_call("canonical-one", "same_name", serde_json::json!({})),
            AssistantContent::tool_call("canonical-two", "same_name", serde_json::json!({})),
        ])
        .unwrap();
        capture.record("run", 1, "native-model", &content);
        capture.tool("run", 1, "canonical-two", None, "internal-two");
        capture.tool("run", 1, "canonical-one", None, "internal-one");
        capture.tool("run", 1, "canonical-one", None, "internal-one");
        let (frames, unavailable) = capture.snapshot();
        assert_eq!(unavailable, None);
        assert_eq!(frames[0].advertised_tools.len(), 2);
        assert_eq!(frames[0].advertised_tools[0].tool_id, "canonical-two");
        assert!(
            frames[0]
                .advertised_tools
                .iter()
                .all(|tool| tool.provider_id.is_none())
        );
        capture.tool("run", 1, "canonical-two", None, "internal-one");
        assert_eq!(capture.snapshot().1, Some(Unavailable::MissingCallIdentity));
        assert!(
            NativeCaptureHook::for_native(None, "native-model")
                .capture
                .is_none()
        );
        assert!(NativeCaptureHook::for_native(None, "native-model").is_native());
        assert!(!NativeCaptureHook::default().is_native());
    }

    #[test]
    fn native_capture_strict_tail_preserves_postrewrite_metadata_and_rejects_identity_reuse() {
        use rig::message::{Text, ToolResult, ToolResultContent};
        let fresh = || {
            let capture = NativeCapture::default();
            let content = OneOrMany::many([
                AssistantContent::tool_call("canonical-one", "same_name", serde_json::json!({})),
                AssistantContent::tool_call("canonical-two", "same_name", serde_json::json!({})),
            ])
            .unwrap();
            capture.record("run", 1, "native-model", &content);
            capture.tool("run", 1, "canonical-two", None, "internal-two");
            capture.tool("run", 1, "canonical-one", None, "internal-one");
            capture
        };
        let first = ToolResult {
            id: "canonical-one".into(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::Text(Text {
                text: "actual replacement and included context notice".into(),
                additional_params: Some(
                    serde_json::json!({"owned_reference_metadata":["exact bytes"]}),
                ),
            })),
        };
        let second = ToolResult {
            id: "canonical-two".into(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::text("actual second result")),
        };
        let capture = fresh();
        capture.tool_result("internal-two", &second);
        assert_eq!(
            capture.snapshot().0[0].native_tool_results,
            vec![second.clone()]
        );
        capture.tool_result("internal-one", &first);
        assert_eq!(
            capture.snapshot().0[0].native_tool_results,
            vec![second.clone(), first.clone()]
        );
        assert_eq!(capture.snapshot().1, None);
        for case in [
            "wrong_internal",
            "unknown",
            "wrong_canonical",
            "wrong_provider",
            "duplicate",
        ] {
            let invalid = fresh();
            let mut result = first.clone();
            let id = match case {
                "wrong_internal" => "internal-two",
                "unknown" => "UNKNOWN_TOOL_OUTCOME",
                "wrong_canonical" => {
                    result.id = "foreign-canonical".into();
                    "internal-one"
                }
                "wrong_provider" => {
                    result.call_id = Some("foreign-provider".into());
                    "internal-one"
                }
                "duplicate" => {
                    invalid.tool_result("internal-one", &result);
                    "internal-one"
                }
                _ => unreachable!(),
            };
            invalid.tool_result(id, &result);
            assert_eq!(
                invalid.snapshot().1,
                Some(Unavailable::MissingCallIdentity),
                "{case}"
            );
            assert_eq!(
                invalid.snapshot().0[0].native_tool_results.len(),
                usize::from(case == "duplicate")
            );
        }
    }

    #[test]
    fn native_capture_recovery_is_explicit_and_sibling_collections_are_isolated() {
        let first = NativeCapture::default();
        let sibling = NativeCapture::default();
        let content = OneOrMany::one(AssistantContent::text("original"));
        first.record("first", 1, "native-model", &content);
        first.unavailable(Unavailable::RecoveredToolContent);
        first.record("first", 2, "native-model", &content);
        sibling.record("sibling", 1, "native-model", &content);
        assert_eq!(first.snapshot().0.len(), 1);
        assert_eq!(first.snapshot().1, Some(Unavailable::RecoveredToolContent));
        assert_eq!(sibling.snapshot().0[0].sdk_run_id, "sibling");
        assert_eq!(sibling.snapshot().1, None);
    }
}
