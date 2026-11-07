//! Per-request image budget (mini-agent-xql2b).
//!
//! Images from `read`, pasted input and MCP screenshots stay in live history
//! verbatim on every request. Past 20 images in one request Anthropic applies a
//! much stricter per-image dimension limit, and every later request fails with a
//! "many-image requests" `invalid_request_error` that auto-compaction does not
//! relieve. [`ImageBudgetHook`] counts the images in each request before it is
//! built and, when over the cap or the encoded-byte budget, replaces the
//! *oldest* ones with a text placeholder for that request only. The persisted
//! transcript and the run state keep the real images.
//!
//! Elision is stateless on the raw history, so it must be stable between turns
//! or every turn would rewrite the prompt prefix and defeat provider caching:
//! the count-based drop is rounded up to a block of [`ELISION_BLOCK`] images.
//! Compaction image evidence is dropped only after every other image.

use rig::agent::{AgentHook, Flow, HookContext, RequestPatch, StepEvent, StepEventKind};
use rig::completion::{CompletionModel, Message};
use rig::message::{AssistantContent, Image, ToolResultContent, UserContent};

/// Most images one request may carry (the provider threshold for the stricter
/// many-image dimension rule).
pub(crate) const MAX_REQUEST_IMAGES: usize = 20;
/// Most encoded image bytes one request may carry; well under the smallest
/// provider request ceiling mini-agent targets.
pub(crate) const MAX_REQUEST_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// Count-based elision drops whole blocks so the request prefix stays stable
/// across consecutive turns.
const ELISION_BLOCK: usize = 5;
const EVIDENCE_PREFIX: &str = "[Historical image evidence from the summarized transcript";

/// Images per request for providers that accept far more than Anthropic's
/// stricter-dimension threshold (OpenAI, Gemini and their gateways).
const WIDE_REQUEST_IMAGES: usize = 100;
/// Most images one prompt may carry before the request is refused up front.
const STRICT_HARD_LIMIT: usize = 100;
const WIDE_HARD_LIMIT: usize = 500;

/// Image limits of one provider/model scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageProfile {
    scope: String,
    default_cap: usize,
    hard_limit: usize,
}

impl ImageProfile {
    /// The profile for `provider`/`model`. Anthropic models (direct, Bedrock,
    /// Vertex, gateways) and unknown providers keep the strict 20-image cap;
    /// OpenAI- and Gemini-style APIs get a wider one.
    pub(crate) fn for_provider(provider: &str, model: &str) -> Self {
        let provider_lower = provider.to_ascii_lowercase();
        let model_lower = model.to_ascii_lowercase();
        let anthropic = model_lower.contains("claude")
            || provider_lower.contains("anthropic")
            || provider_lower.contains("bedrock");
        let wide = !anthropic
            && [
                "openai", "gemini", "google", "azure", "copilot", "xai", "grok",
            ]
            .iter()
            .any(|name| provider_lower.contains(name));
        Self {
            scope: format!("{provider_lower}/{model_lower}"),
            default_cap: if wide {
                WIDE_REQUEST_IMAGES
            } else {
                MAX_REQUEST_IMAGES
            },
            hard_limit: if wide {
                WIDE_HARD_LIMIT
            } else {
                STRICT_HARD_LIMIT
            },
        }
    }
}

impl Default for ImageProfile {
    fn default() -> Self {
        Self {
            scope: String::new(),
            default_cap: MAX_REQUEST_IMAGES,
            hard_limit: STRICT_HARD_LIMIT,
        }
    }
}

/// Caps lowered after a provider rejection, per provider/model scope, so one
/// Anthropic rejection never degrades an OpenAI or Gemini session.
static TIGHTENED: std::sync::Mutex<Vec<(String, usize)>> = std::sync::Mutex::new(Vec::new());
/// `(scope, default cap)` of the request being built most recently.
static ACTIVE: std::sync::Mutex<Option<(String, usize)>> = std::sync::Mutex::new(None);
/// User-visible elision notices not yet shown.
static NOTICES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static LAST_NOTICE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn scope_cap(scope: &str, default_cap: usize) -> usize {
    lock(&TIGHTENED)
        .iter()
        .find(|(name, _)| name == scope)
        .map_or(default_cap, |(_, cap)| (*cap).min(default_cap))
}

/// Effective cap of the active provider/model scope.
#[cfg(test)]
pub(crate) fn image_cap() -> usize {
    let (scope, default_cap) = lock(&ACTIVE)
        .clone()
        .unwrap_or_else(|| (String::new(), MAX_REQUEST_IMAGES));
    scope_cap(&scope, default_cap)
}

/// A provider rejected the request for its image count or dimensions: keep
/// fewer images from now on for the active scope only. Returns whether the
/// cap actually shrank.
pub(crate) fn tighten_after_provider_rejection() -> bool {
    let (scope, default_cap) = lock(&ACTIVE)
        .clone()
        .unwrap_or_else(|| (String::new(), MAX_REQUEST_IMAGES));
    let current = scope_cap(&scope, default_cap);
    if current <= 1 {
        return false;
    }
    let next = (current / 2).max(1);
    let mut tightened = lock(&TIGHTENED);
    match tightened.iter_mut().find(|(name, _)| *name == scope) {
        Some(entry) => entry.1 = next,
        None => tightened.push((scope, next)),
    }
    true
}

/// Elision notices queued since the last call; the runner shows them to the
/// user (TUI, headless stderr, ACP) once per request round.
pub(crate) fn take_notices() -> Vec<String> {
    std::mem::take(&mut *lock(&NOTICES))
}

fn queue_notice(text: String) {
    let mut last = lock(&LAST_NOTICE);
    if *last == text {
        return;
    }
    last.clone_from(&text);
    let mut notices = lock(&NOTICES);
    if notices.len() < 8 {
        notices.push(text);
    }
}

/// Serializes tests that read or move the process-wide state.
#[cfg(test)]
pub(crate) static CAP_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) fn reset_cap_for_tests() {
    lock(&TIGHTENED).clear();
    *lock(&ACTIVE) = None;
    lock(&NOTICES).clear();
    lock(&LAST_NOTICE).clear();
}

/// Provider message for an over-limit image request ("many-image requests",
/// "too many images", ...), as opposed to a context-length error.
pub(crate) fn is_image_limit_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("many-image")
        || lower.contains("too many images")
        || lower.contains("maximum number of images")
        || lower.contains("max number of images")
        || (lower.contains("images") && lower.contains("at most") && lower.contains("per request"))
}

fn placeholder(cap: usize) -> String {
    format!(
        "[image omitted: an older image was dropped from this request to stay within the per-request image budget (at most {cap} images, {} MiB encoded). Re-read the file or re-capture the screen if it is still needed.]",
        MAX_REQUEST_IMAGE_BYTES / (1024 * 1024)
    )
}

fn encoded_len(image: &Image) -> usize {
    use rig::message::DocumentSourceKind as Kind;
    match &image.data {
        Kind::Base64(data) | Kind::String(data) => data.len(),
        Kind::Raw(bytes) => bytes.len(),
        _ => 0,
    }
}

fn is_evidence(message: &Message) -> bool {
    matches!(message, Message::User { content }
        if matches!(content.first_ref(), UserContent::Text(text) if text.text.starts_with(EVIDENCE_PREFIX)))
}

/// `(encoded bytes, protected evidence)` of every image, in request order.
fn scan(messages: &[Message]) -> Vec<(usize, bool)> {
    let mut images = Vec::new();
    for message in messages {
        let evidence = is_evidence(message);
        match message {
            Message::User { content } => {
                for item in content.iter() {
                    match item {
                        UserContent::Image(image) => images.push((encoded_len(image), evidence)),
                        UserContent::ToolResult(result) => {
                            for inner in result.content.iter() {
                                if let ToolResultContent::Image(image) = inner {
                                    images.push((encoded_len(image), false));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for item in content.iter() {
                    if let AssistantContent::Image(image) = item {
                        images.push((encoded_len(image), false));
                    }
                }
            }
            _ => {}
        }
    }
    images
}

/// Replace the images whose request-order ordinals are in `elide` by text.
fn replace(messages: &mut [Message], elide: &[bool], note: &str) {
    let mut ordinal = 0usize;
    let take = |ordinal: &mut usize| {
        let drop = elide.get(*ordinal).copied().unwrap_or(false);
        *ordinal += 1;
        drop
    };
    for message in messages {
        match message {
            Message::User { content } => {
                for item in content.iter_mut() {
                    match item {
                        UserContent::Image(_) if take(&mut ordinal) => {
                            *item = UserContent::text(note);
                        }
                        UserContent::ToolResult(result) => {
                            for inner in result.content.iter_mut() {
                                if matches!(inner, ToolResultContent::Image(_))
                                    && take(&mut ordinal)
                                {
                                    *inner = ToolResultContent::text(note);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for item in content.iter_mut() {
                    if matches!(item, AssistantContent::Image(_)) && take(&mut ordinal) {
                        *item = AssistantContent::text(note);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The history to send instead of `history` when the request (history plus the
/// `prompt`) exceeds the budget, with how many images were replaced; `None`
/// when it already fits. The prompt is the newest message and is never touched.
pub(crate) fn enforce(
    history: &[Message],
    prompt: &Message,
    cap: usize,
    max_bytes: usize,
) -> Option<(Vec<Message>, usize)> {
    let history_images = scan(history);
    let prompt_images = scan(std::slice::from_ref(prompt));
    let total = history_images.len() + prompt_images.len();
    let mut bytes = history_images.iter().map(|image| image.0).sum::<usize>()
        + prompt_images.iter().map(|image| image.0).sum::<usize>();
    if total <= cap && bytes <= max_bytes {
        return None;
    }
    // Oldest first, compaction evidence last.
    let order = history_images
        .iter()
        .enumerate()
        .filter(|(_, image)| !image.1)
        .chain(
            history_images
                .iter()
                .enumerate()
                .filter(|(_, image)| image.1),
        )
        .map(|(ordinal, image)| (ordinal, image.0))
        .collect::<Vec<_>>();
    let count_drop = if total > cap {
        (total - cap).div_ceil(ELISION_BLOCK) * ELISION_BLOCK
    } else {
        0
    };
    let mut elide = vec![false; history_images.len()];
    let mut dropped = 0usize;
    for (ordinal, size) in order {
        if dropped >= count_drop && bytes <= max_bytes {
            break;
        }
        elide[ordinal] = true;
        dropped += 1;
        bytes -= size;
    }
    if dropped == 0 {
        return None;
    }
    let mut patched = history.to_vec();
    replace(&mut patched, &elide, &placeholder(cap));
    Some((patched, dropped))
}

/// Applies the per-request image budget to every completion call.
#[derive(Clone, Default)]
pub(crate) struct ImageBudgetHook {
    profile: ImageProfile,
}

impl ImageBudgetHook {
    pub(crate) fn for_provider(provider: &str, model: &str) -> Self {
        Self {
            profile: ImageProfile::for_provider(provider, model),
        }
    }
}

/// Whether the newest message alone cannot be sent: it exceeds the provider's
/// hard image limit, the byte budget, or a cap a provider rejection lowered.
fn prompt_refusal(prompt: &Message, cap: usize, profile: &ImageProfile) -> Option<String> {
    let images = scan(std::slice::from_ref(prompt));
    let bytes = images.iter().map(|image| image.0).sum::<usize>();
    let tightened = cap < profile.default_cap;
    let over_count = images.len() > profile.hard_limit || (tightened && images.len() > cap);
    let over_bytes = bytes > MAX_REQUEST_IMAGE_BYTES;
    if !over_count && !over_bytes {
        return None;
    }
    Some(format!(
        "This message carries {} images ({} MiB encoded), more than the provider accepts in one request (limit {} images, {} MiB). Send fewer or smaller images and try again; older images in the saved session are unaffected.",
        images.len(),
        bytes / (1024 * 1024),
        cap.min(profile.hard_limit),
        MAX_REQUEST_IMAGE_BYTES / (1024 * 1024)
    ))
}

impl<M: CompletionModel> AgentHook<M> for ImageBudgetHook {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        let StepEvent::CompletionCall {
            prompt, history, ..
        } = event
        else {
            return Flow::Continue;
        };
        *lock(&ACTIVE) = Some((self.profile.scope.clone(), self.profile.default_cap));
        let cap = scope_cap(&self.profile.scope, self.profile.default_cap);
        if let Some(reason) = prompt_refusal(prompt, cap, &self.profile) {
            return Flow::terminate(reason);
        }
        match enforce(history, prompt, cap, MAX_REQUEST_IMAGE_BYTES) {
            Some((patched, dropped)) => {
                tracing::warn!(
                    dropped,
                    cap,
                    "older images replaced by a placeholder to stay within the per-request image budget"
                );
                queue_notice(format!(
                    "{dropped} older image(s) were left out of this request to stay within the provider's image budget (at most {cap} images, {} MiB). They remain in the saved session; re-read the file or re-capture the screen if needed.",
                    MAX_REQUEST_IMAGE_BYTES / (1024 * 1024)
                ));
                Flow::patch_request(RequestPatch::new().history(patched))
            }
            None => Flow::Continue,
        }
    }
    fn observes(&self, kind: StepEventKind) -> bool {
        kind == StepEventKind::CompletionCall
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::OneOrMany;
    use rig::message::{DocumentSourceKind, ImageMediaType, ToolResult};
    use rig::streaming::StreamingChat;

    fn image(len: usize) -> Image {
        Image {
            data: DocumentSourceKind::Base64("A".repeat(len)),
            media_type: Some(ImageMediaType::PNG),
            detail: None,
            additional_params: None,
        }
    }

    fn tool_image_message(id: usize, len: usize) -> Message {
        Message::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: format!("call-{id}"),
                call_id: None,
                content: OneOrMany::many(vec![
                    ToolResultContent::text(format!("read {id}")),
                    ToolResultContent::Image(image(len)),
                ])
                .unwrap(),
            })),
        }
    }

    fn count(messages: &[Message]) -> usize {
        scan(messages).len()
    }

    fn texts(messages: &[Message]) -> Vec<String> {
        let mut out = Vec::new();
        for message in messages {
            if let Message::User { content } = message {
                for item in content.iter() {
                    if let UserContent::ToolResult(result) = item {
                        for inner in result.content.iter() {
                            if let ToolResultContent::Text(text) = inner {
                                out.push(text.text.clone());
                            }
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn under_budget_is_untouched() {
        let history: Vec<_> = (0..19).map(|id| tool_image_message(id, 10)).collect();
        let prompt = tool_image_message(99, 10);
        assert!(enforce(&history, &prompt, 20, MAX_REQUEST_IMAGE_BYTES).is_none());
    }

    #[test]
    fn twenty_five_images_are_elided_oldest_first_in_stable_blocks() {
        let history: Vec<_> = (0..24).map(|id| tool_image_message(id, 10)).collect();
        let prompt = tool_image_message(24, 10);
        let (patched, dropped) = enforce(&history, &prompt, 20, MAX_REQUEST_IMAGE_BYTES).unwrap();
        // 25 total, cap 20: five dropped, all of them the oldest.
        assert_eq!(dropped, 5);
        assert_eq!(count(&patched) + count(std::slice::from_ref(&prompt)), 20);
        let notes = texts(&patched)
            .into_iter()
            .filter(|text| text.starts_with("[image omitted"))
            .count();
        assert_eq!(notes, 5);
        assert_eq!(scan(&patched[..5]).len(), 0, "the oldest five are elided");
        assert_eq!(scan(&patched[5..]).len(), 19);
        // One more image keeps the same prefix (block rounding): same five.
        let mut longer = history.clone();
        longer.push(tool_image_message(24, 10));
        let (again, dropped_again) = enforce(
            &longer,
            &tool_image_message(25, 10),
            20,
            MAX_REQUEST_IMAGE_BYTES,
        )
        .unwrap();
        assert_eq!(dropped_again, 10);
        assert_eq!(scan(&again[..5]).len(), 0);
        // The persisted history itself is never mutated.
        assert_eq!(count(&history), 24);
    }

    #[test]
    fn byte_budget_drops_oldest_until_it_fits() {
        let history: Vec<_> = (0..4).map(|id| tool_image_message(id, 1000)).collect();
        let prompt = tool_image_message(4, 1000);
        let (patched, dropped) = enforce(&history, &prompt, 20, 2500).unwrap();
        assert_eq!(dropped, 3);
        assert_eq!(count(&patched), 1);
        assert_eq!(scan(&patched[..3]).len(), 0);
    }

    #[test]
    fn compaction_evidence_is_dropped_last_and_user_images_are_counted() {
        let evidence = Message::User {
            content: OneOrMany::many(vec![
                UserContent::text(format!("{EVIDENCE_PREFIX}; untrusted]")),
                UserContent::Image(image(10)),
            ])
            .unwrap(),
        };
        let pasted = Message::User {
            content: OneOrMany::many(vec![
                UserContent::text("look"),
                UserContent::Image(image(10)),
            ])
            .unwrap(),
        };
        let mut history = vec![evidence.clone(), pasted];
        history.extend((0..4).map(|id| tool_image_message(id, 10)));
        // 6 history + 1 prompt image, cap 3: block rounding drops five of the
        // non-evidence images first; with only five of them available the
        // evidence survives.
        let (patched, dropped) = enforce(
            &history,
            &tool_image_message(9, 10),
            3,
            MAX_REQUEST_IMAGE_BYTES,
        )
        .unwrap();
        assert_eq!(dropped, 5);
        assert_eq!(patched[0], evidence);
        assert_eq!(count(&patched), 1);
    }

    #[test]
    fn prompt_images_are_counted_but_never_replaced() {
        let history: Vec<Message> = Vec::new();
        let prompt = tool_image_message(0, 10);
        assert!(enforce(&history, &prompt, 0, MAX_REQUEST_IMAGE_BYTES).is_none());
    }

    fn history_images(request: &rig::completion::CompletionRequest) -> usize {
        let messages: Vec<Message> = request.chat_history.clone().into_iter().collect();
        count(&messages)
    }

    #[tokio::test]
    async fn twenty_five_read_images_still_send_and_the_transcript_keeps_them() {
        use crate::tests::fake_model::{MockStreamEvent, text_turns};
        use futures::StreamExt;
        let _guard = CAP_TEST_LOCK.lock().await;
        reset_cap_for_tests();
        let model = text_turns([["ok"]]);
        let agent = rig::agent::AgentBuilder::new(model.clone()).build();
        let history: Vec<Message> = (0..24).map(|id| tool_image_message(id, 64)).collect();
        let mut stream = agent
            .stream_chat(tool_image_message(24, 64), history.clone())
            .add_hook(ImageBudgetHook::default())
            .await;
        while let Some(item) = stream.next().await {
            item.expect("request must be sent");
        }
        let requests = model.requests();
        assert_eq!(requests.len(), 1);
        assert!(history_images(&requests[0]) <= MAX_REQUEST_IMAGES);
        assert!(history_images(&requests[0]) >= MAX_REQUEST_IMAGES - ELISION_BLOCK);
        assert_eq!(count(&history), 24, "the caller's history is not mutated");
        let _ = MockStreamEvent::text("unused");
    }

    /// Steering injects input in the same turn the budget replaces history.
    struct InjectOnTurnTwo;
    impl<M: CompletionModel> AgentHook<M> for InjectOnTurnTwo {
        async fn on_event(&self, _: &HookContext, event: StepEvent<'_, M>) -> Flow {
            if matches!(event, StepEvent::CompletionCall { turn: 2, .. }) {
                Flow::patch_request(RequestPatch::new().append_input([Message::user("steer")]))
            } else {
                Flow::Continue
            }
        }
    }

    #[tokio::test]
    async fn budget_and_steering_in_one_turn_both_apply() {
        use crate::tests::fake_model::MockStreamEvent;
        use futures::StreamExt;
        use rig::test_utils::MockCompletionModel;
        let _guard = CAP_TEST_LOCK.lock().await;
        reset_cap_for_tests();
        struct Probe;
        impl rig::tool::Tool for Probe {
            const NAME: &'static str = "probe";
            type Error = std::convert::Infallible;
            type Args = serde_json::Value;
            type Output = String;
            fn description(&self) -> String {
                "probe".into()
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            async fn call(&self, _: Self::Args) -> Result<String, Self::Error> {
                Ok("probed".into())
            }
        }
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("p1", "probe", serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = rig::agent::AgentBuilder::new(model.clone())
            .tool(Probe)
            .default_max_turns(3)
            .build();
        let history: Vec<Message> = (0..25).map(|id| tool_image_message(id, 64)).collect();
        let mut stream = agent
            .stream_chat(Message::user("go"), history)
            .add_hook(ImageBudgetHook::default())
            .add_hook(InjectOnTurnTwo)
            .await;
        while let Some(item) = stream.next().await {
            item.expect("budget plus steering must not fail the run");
        }
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert!(history_images(request) <= MAX_REQUEST_IMAGES);
        }
        let last = requests[1].chat_history.last_ref();
        assert_eq!(last, &Message::user("steer"));
    }

    #[test]
    fn provider_rejection_halves_the_cap_and_is_recognised() {
        let _guard = CAP_TEST_LOCK.blocking_lock();
        reset_cap_for_tests();
        assert!(is_image_limit_error_message(
            "image dimensions exceed max allowed size for many-image requests: 2000 pixels"
        ));
        assert!(is_image_limit_error_message("Too many images in request"));
        assert!(!is_image_limit_error_message("prompt is too long"));
        assert!(tighten_after_provider_rejection());
        assert_eq!(image_cap(), MAX_REQUEST_IMAGES / 2);
        reset_cap_for_tests();
    }

    fn activate(profile: &ImageProfile) {
        *lock(&ACTIVE) = Some((profile.scope.clone(), profile.default_cap));
    }

    #[test]
    fn a_rejection_on_one_provider_does_not_lower_another_providers_cap() {
        let _guard = CAP_TEST_LOCK.blocking_lock();
        reset_cap_for_tests();
        let anthropic = ImageProfile::for_provider("anthropic", "claude-sonnet-4");
        let openai = ImageProfile::for_provider("openai", "gpt-5");
        assert_eq!(anthropic.default_cap, MAX_REQUEST_IMAGES);
        assert_eq!(openai.default_cap, WIDE_REQUEST_IMAGES);
        // A Claude model behind a gateway stays strict.
        assert_eq!(
            ImageProfile::for_provider("openai", "anthropic/claude-x").default_cap,
            MAX_REQUEST_IMAGES
        );
        activate(&anthropic);
        assert!(tighten_after_provider_rejection());
        assert_eq!(image_cap(), MAX_REQUEST_IMAGES / 2);
        activate(&openai);
        assert_eq!(image_cap(), WIDE_REQUEST_IMAGES);
        activate(&anthropic);
        assert_eq!(image_cap(), MAX_REQUEST_IMAGES / 2);
        reset_cap_for_tests();
    }

    #[tokio::test]
    async fn elision_queues_one_user_visible_notice_and_dedupes() {
        use crate::tests::fake_model::text_turns;
        use futures::StreamExt;
        let _guard = CAP_TEST_LOCK.lock().await;
        reset_cap_for_tests();
        let model = text_turns([["ok"]]);
        let agent = rig::agent::AgentBuilder::new(model.clone()).build();
        let history: Vec<Message> = (0..24).map(|id| tool_image_message(id, 64)).collect();
        let mut stream = agent
            .stream_chat(tool_image_message(24, 64), history)
            .add_hook(ImageBudgetHook::for_provider("anthropic", "claude-x"))
            .await;
        while let Some(item) = stream.next().await {
            item.expect("request must be sent");
        }
        let notices = take_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("older image(s) were left out"));
        assert!(take_notices().is_empty());
        queue_notice("same".into());
        queue_notice("same".into());
        assert_eq!(take_notices().len(), 1);
        reset_cap_for_tests();
    }

    #[tokio::test]
    async fn a_prompt_over_the_hard_limit_or_a_tightened_cap_is_refused_clearly() {
        use crate::tests::fake_model::text_turns;
        use futures::StreamExt;
        let _guard = CAP_TEST_LOCK.lock().await;
        reset_cap_for_tests();
        let profile = ImageProfile::for_provider("anthropic", "claude-x");
        // Just above the soft cap: still sent (history elision only).
        assert!(prompt_refusal(&tool_image_message(0, 8), 20, &profile).is_none());
        let many = Message::User {
            content: OneOrMany::many(
                (0..STRICT_HARD_LIMIT + 1).map(|_| UserContent::Image(image(8))),
            )
            .unwrap(),
        };
        assert!(
            prompt_refusal(&many, 20, &profile)
                .unwrap()
                .contains("101 images")
        );
        let few = Message::User {
            content: OneOrMany::many((0..6).map(|_| UserContent::Image(image(8)))).unwrap(),
        };
        assert!(prompt_refusal(&few, 20, &profile).is_none());
        assert!(prompt_refusal(&few, 5, &profile).is_some(), "tightened cap");
        let huge = Message::User {
            content: OneOrMany::one(UserContent::Image(image(MAX_REQUEST_IMAGE_BYTES + 1))),
        };
        assert!(prompt_refusal(&huge, 20, &profile).is_some());
        // End to end: the run is refused before any provider request.
        let model = text_turns([["ok"]]);
        let agent = rig::agent::AgentBuilder::new(model.clone()).build();
        let mut stream = agent
            .stream_chat(many, Vec::<Message>::new())
            .add_hook(ImageBudgetHook::for_provider("anthropic", "claude-x"))
            .await;
        let mut failed = false;
        while let Some(item) = stream.next().await {
            failed |= item.is_err();
        }
        assert!(failed);
        assert!(model.requests().is_empty());
        reset_cap_for_tests();
    }
}
