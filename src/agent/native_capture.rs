//! Bounded, invocation-owned observation of accepted native SDK content.
//!
//! This is compatibility data, never tool/security authority. No Session replay
//! policy is enabled here; the caller must later prove the actual request prefix.

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};

use rig::OneOrMany;
use rig::agent::{AgentHook, Flow, HookContext, StepEvent, StepEventKind};
use rig::completion::{AssistantContent, CompletionModel};

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

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct AdvertisedToolIdentity {
    pub(crate) tool_id: String,
    pub(crate) provider_id: Option<String>,
    pub(crate) internal_id: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct NativeTurnProjection {
    pub(crate) sdk_run_id: String,
    pub(crate) turn: usize,
    /// Read from the actual native typed model, not inferred from its name.
    /// Raw request overrides/prefix compatibility are deliberately not proven.
    pub(crate) configured_model: String,
    pub(crate) content: OneOrMany<AssistantContent>,
    pub(crate) advertised_tools: Vec<AdvertisedToolIdentity>,
}

#[derive(Default)]
struct State {
    turns: Vec<NativeTurnProjection>,
    keys: HashSet<(String, usize)>,
    bytes: usize,
    unavailable: Option<Unavailable>,
}

#[derive(Clone, Default)]
pub(crate) struct NativeCapture(Arc<Mutex<State>>);

impl NativeCapture {
    fn unavailable(&self, reason: Unavailable) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unavailable
            .get_or_insert(reason);
    }

    fn record(
        &self,
        run_id: &str,
        turn: usize,
        model: &str,
        content: &OneOrMany<AssistantContent>,
    ) {
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
        state.turns.push(NativeTurnProjection {
            sdk_run_id: run_id.to_string(),
            turn,
            configured_model: model.to_string(),
            content: content.clone(),
            advertised_tools: Vec::new(),
        });
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
pub(crate) struct NativeCaptureHook(Option<(NativeCapture, String)>);

impl NativeCaptureHook {
    pub(crate) fn for_native(capture: Option<NativeCapture>, model: &str) -> Self {
        if model.len() > MAX_IDENTITY_BYTES {
            if let Some(capture) = capture {
                capture.unavailable(Unavailable::Limit);
            }
            return Self::default();
        }
        Self(capture.map(|capture| (capture, model.to_string())))
    }
}

impl<M: CompletionModel> AgentHook<M> for NativeCaptureHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        self.0.is_some()
            && matches!(
                kind,
                StepEventKind::ModelTurnFinished
                    | StepEventKind::InvalidToolCall
                    | StepEventKind::ToolCall
            )
    }
    async fn on_event(&self, ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        if let Some((capture, model)) = &self.0 {
            match event {
                StepEvent::ModelTurnFinished { turn, content, .. } => {
                    capture.record(ctx.run_id().as_str(), turn, model, content)
                }
                StepEvent::InvalidToolCall(_) => {
                    capture.unavailable(Unavailable::RecoveredToolContent)
                }
                StepEvent::ToolCall {
                    tool_id,
                    tool_call_id,
                    internal_call_id,
                    ..
                } => capture.tool(
                    ctx.run_id().as_str(),
                    ctx.turn(),
                    tool_id,
                    tool_call_id,
                    internal_call_id,
                ),
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
                .0
                .is_none()
        );
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
