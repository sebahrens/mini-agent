//! Token estimates and an aggregate byte ceiling for images inside a child's
//! history. Provider-reported usage stays authoritative for cost units; these
//! estimates replace the base64 text length only in the local request estimate
//! used when a provider omits usage and by the child context guard.

use base64::Engine;
use rig::message::{DocumentSourceKind, Message, ToolResultContent, UserContent};

/// Edge of one vision patch in Claude's published formula.
const PATCH_PX: u64 = 28;
/// Highest per-image charge: Claude's high-resolution tier (the largest
/// published per-image figure among the supported providers). Providers
/// downscale larger images, so the raw patch count of an 8000 px image would
/// overcharge it by an order of magnitude.
pub(crate) const MAX_IMAGE_TOKENS: u64 = 4_784;
/// Assumed compressed density when dimensions are unknown: four pixels per
/// decoded byte, deliberately dense so that the byte fallback overcharges.
const FALLBACK_PIXELS_PER_BYTE: u64 = 4;
/// Aggregate base64 bytes of images one child may hold in its history, the
/// same 4 MiB aggregate payload ceiling native capture applies
/// (`docs/agent/IMAGE_READ.md`).
pub(crate) const MAX_CHILD_IMAGE_ENCODED_BYTES: u64 = 4 * 1024 * 1024;

/// `ceil(w/28) * ceil(h/28)`, capped at [`MAX_IMAGE_TOKENS`].
pub(crate) fn patch_tokens(width: u32, height: u32) -> u64 {
    (u64::from(width).div_ceil(PATCH_PX) * u64::from(height).div_ceil(PATCH_PX))
        .clamp(1, MAX_IMAGE_TOKENS)
}

/// Token estimate of one literal base64 image: the patch formula from its
/// header dimensions, else a byte-based estimate of the same formula.
pub(crate) fn base64_image_tokens(data: &str) -> u64 {
    let decoded = base64::engine::general_purpose::STANDARD.decode(data.as_bytes());
    if let Ok(bytes) = &decoded
        && let Ok(Some(info)) = crate::agent::tools::read_image::inspect(bytes)
    {
        return patch_tokens(info.width, info.height);
    }
    let decoded_len = decoded.map_or((data.len() as u64).saturating_mul(3) / 4, |bytes| {
        bytes.len() as u64
    });
    (decoded_len.saturating_mul(FALLBACK_PIXELS_PER_BYTE))
        .div_ceil(PATCH_PX * PATCH_PX)
        .clamp(1, MAX_IMAGE_TOKENS)
}

fn image_cost(image: &rig::message::Image) -> ImageCost {
    match &image.data {
        DocumentSourceKind::Base64(data) => ImageCost {
            encoded_chars: data.chars().count() as u64,
            tokens: base64_image_tokens(data),
        },
        // A reference has no local bytes; charge the per-image maximum.
        _ => ImageCost {
            encoded_chars: 0,
            tokens: MAX_IMAGE_TOKENS,
        },
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImageCost {
    /// Characters of literal base64 payload inside the serialized message.
    pub(crate) encoded_chars: u64,
    /// Estimated model tokens of those images.
    pub(crate) tokens: u64,
}

impl std::ops::AddAssign for ImageCost {
    fn add_assign(&mut self, other: Self) {
        self.encoded_chars = self.encoded_chars.saturating_add(other.encoded_chars);
        self.tokens = self.tokens.saturating_add(other.tokens);
    }
}

/// Images carried by one history message: user images and tool-result images.
pub(crate) fn message_image_cost(message: &Message) -> ImageCost {
    let mut cost = ImageCost::default();
    if let Message::User { content } = message {
        for item in content.iter() {
            match item {
                UserContent::Image(image) => cost += image_cost(image),
                UserContent::ToolResult(result) => {
                    for part in result.content.iter() {
                        if let ToolResultContent::Image(image) = part {
                            cost += image_cost(image);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    cost
}

/// Base64 bytes of the images in a raw tool output envelope
/// (`{"response": .., "parts": [{"type": "image", "data": .., ..}]}`), or
/// `None` when the output is not such an envelope.
fn envelope_image_bytes(output: &str) -> Option<(u64, serde_json::Value)> {
    let value = serde_json::from_str::<serde_json::Value>(output).ok()?;
    let parts = value.get("parts")?.as_array()?;
    let bytes = parts
        .iter()
        .filter(|part| part["type"] == "image")
        .filter_map(|part| part["data"].as_str())
        .fold(0u64, |total, data| total.saturating_add(data.len() as u64));
    (bytes > 0).then_some((bytes, value))
}

/// Run-scoped running total of admitted image bytes.
#[derive(Clone, Copy, Default)]
struct AdmittedImageBytes(u64);

/// Drops the images of a child tool result that would take the child's
/// history over [`MAX_CHILD_IMAGE_ENCODED_BYTES`], keeping its text and telling
/// the child why. The total is run-scoped, so a provider retry starts afresh.
#[derive(Clone, Copy)]
pub(crate) struct ChildImageCeiling {
    pub(crate) max_encoded_bytes: u64,
}

impl Default for ChildImageCeiling {
    fn default() -> Self {
        Self {
            max_encoded_bytes: MAX_CHILD_IMAGE_ENCODED_BYTES,
        }
    }
}

impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for ChildImageCeiling {
    async fn on_event(
        &self,
        ctx: &rig::agent::HookContext,
        event: rig::agent::StepEvent<'_, M>,
    ) -> rig::agent::Flow {
        let rig::agent::StepEvent::ToolResult { result, .. } = event else {
            return rig::agent::Flow::cont();
        };
        let Some((bytes, value)) = envelope_image_bytes(result) else {
            return rig::agent::Flow::cont();
        };
        let admitted = ctx
            .scratchpad()
            .get::<AdmittedImageBytes>()
            .unwrap_or_default()
            .0;
        let total = admitted.saturating_add(bytes);
        if total <= self.max_encoded_bytes {
            ctx.scratchpad().insert(AdmittedImageBytes(total));
            return rig::agent::Flow::cont();
        }
        let response = match &value["response"] {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        rig::agent::Flow::rewrite_result(format!(
            "{response}\n[image omitted: this subagent's images would total {total} encoded bytes, over its {} byte ceiling; describe what you need from text or read a smaller image]",
            self.max_encoded_bytes
        ))
    }

    fn observes(&self, kind: rig::agent::StepEventKind) -> bool {
        kind == rig::agent::StepEventKind::ToolResult
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use rig::agent::AgentBuilder;
    use rig::streaming::StreamingChat;
    use rig::test_utils::{MockCompletionModel, MockStreamEvent, MockToolError};
    use rig::tool::Tool;

    fn png_base64() -> String {
        base64::engine::general_purpose::STANDARD.encode(include_bytes!(
            "../../agent/tools/fixtures/read-image64/3x2.png"
        ))
    }

    #[test]
    fn patch_formula_matches_claude_and_is_capped() {
        assert_eq!(patch_tokens(3, 2), 1);
        assert_eq!(patch_tokens(28, 28), 1);
        assert_eq!(patch_tokens(29, 28), 2);
        // 1092x1092 is Claude's ~1.5k-token standard example: 39*39.
        assert_eq!(patch_tokens(1092, 1092), 1_521);
        assert_eq!(patch_tokens(8_000, 8_000), MAX_IMAGE_TOKENS);
    }

    #[test]
    fn base64_estimate_uses_header_dimensions_else_bytes() {
        assert_eq!(base64_image_tokens(&png_base64()), 1);
        // 196 KiB of non-image bytes: 200_704 * 4 / 784 = 1_024 tokens.
        let opaque = base64::engine::general_purpose::STANDARD.encode(vec![7u8; 200_704]);
        assert_eq!(base64_image_tokens(&opaque), 1_024);
        let huge = base64::engine::general_purpose::STANDARD.encode(vec![7u8; 4 << 20]);
        assert_eq!(base64_image_tokens(&huge), MAX_IMAGE_TOKENS);
    }

    #[test]
    fn message_image_cost_counts_tool_result_and_user_images() {
        let data = png_base64();
        let image = rig::message::Image {
            data: DocumentSourceKind::Base64(data.clone()),
            media_type: Some(rig::message::ImageMediaType::PNG),
            detail: None,
            additional_params: None,
        };
        let message = Message::User {
            content: rig::OneOrMany::many(vec![
                UserContent::Image(image.clone()),
                UserContent::ToolResult(rig::message::ToolResult {
                    id: "t".into(),
                    call_id: None,
                    content: rig::OneOrMany::many(vec![
                        ToolResultContent::text("summary"),
                        ToolResultContent::Image(image),
                    ])
                    .unwrap(),
                }),
            ])
            .unwrap(),
        };
        assert_eq!(
            message_image_cost(&message),
            ImageCost {
                encoded_chars: 2 * data.len() as u64,
                tokens: 2,
            }
        );
        assert_eq!(
            message_image_cost(&Message::user("no images")),
            ImageCost::default()
        );
    }

    #[derive(Clone)]
    struct ImageTool;

    impl Tool for ImageTool {
        const NAME: &'static str = "image_tool";
        type Error = MockToolError;
        type Args = serde_json::Value;
        type Output = String;
        fn description(&self) -> String {
            "Return an image envelope".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
            Ok(serde_json::json!({
                "response": "PNG 3x2",
                "parts": [{"type": "image", "mimeType": "image/png", "data": png_base64()}],
            })
            .to_string())
        }
    }

    /// Two image results; the ceiling admits exactly one of them.
    async fn run_two_image_calls(ceiling: u64) -> Vec<rig::message::ToolResult> {
        let call =
            |id: &str| MockStreamEvent::tool_call(id, ImageTool::NAME, serde_json::json!({}));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                call("a"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                call("b"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model)
            .tool(ImageTool)
            .add_hook(ChildImageCeiling {
                max_encoded_bytes: ceiling,
            })
            .build();
        let mut stream = agent
            .stream_chat("start", Vec::<Message>::new())
            .max_turns(3)
            .await;
        let mut results = Vec::new();
        while let Some(item) = stream.next().await {
            if let Ok(rig::agent::MultiTurnStreamItem::StreamUserItem(
                rig::streaming::StreamedUserContent::ToolResult { tool_result, .. },
            )) = item
            {
                results.push(tool_result);
            }
        }
        results
    }

    fn image_count(result: &rig::message::ToolResult) -> usize {
        result
            .content
            .iter()
            .filter(|part| matches!(part, ToolResultContent::Image(_)))
            .count()
    }

    /// The prepared-request fallback that cost units use when the provider
    /// omits usage charges an image by its patch estimate, not its base64.
    #[tokio::test]
    async fn fallback_estimate_charges_images_by_patch_tokens() {
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("a", ImageTool::NAME, serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone()).tool(ImageTool).build();
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let admission = crate::agent::runner::SubagentAdmission::new(|| true, {
            let observed = observed.clone();
            move |_, fallback| observed.lock().unwrap().push(fallback.input_tokens)
        });
        let ledger = crate::agent::runner::SharedUsageLedger::default().with_admission(admission);
        let mut stream = agent
            .stream_chat("brief", Vec::<Message>::new())
            .max_turns(2)
            .add_hook(ledger.clone())
            .await;
        while let Some(item) = stream.next().await {
            if let Ok(rig::agent::MultiTurnStreamItem::CompletionCall(call)) = item {
                ledger.record_completion(call.usage);
            }
        }
        let requests = model.requests();
        let fallbacks = observed.lock().unwrap().clone();
        assert_eq!(fallbacks.len(), 2);
        // Image-free first request: unchanged text estimate.
        assert_eq!(
            fallbacks[0],
            crate::session::Session::estimate_tokens(&serde_json::to_string(&requests[0]).unwrap())
        );
        // Second request: base64 excluded, one patch token added.
        let (wide, narrow) =
            crate::session::Session::char_counts(&serde_json::to_string(&requests[1]).unwrap());
        let b64 = png_base64().len() as u64;
        assert_eq!(
            fallbacks[1],
            crate::session::Session::estimate_tokens_from_counts(wide, narrow - b64) + 1
        );
        assert!(
            fallbacks[1]
                < crate::session::Session::estimate_tokens(
                    &serde_json::to_string(&requests[1]).unwrap()
                )
        );
    }

    #[tokio::test]
    async fn child_image_ceiling_drops_images_past_the_aggregate_and_keeps_text() {
        let one = png_base64().len() as u64;
        let results = run_two_image_calls(one + one / 2).await;
        assert_eq!(results.len(), 2);
        assert_eq!(image_count(&results[0]), 1);
        assert_eq!(image_count(&results[1]), 0);
        let ToolResultContent::Text(text) = results[1].content.first() else {
            panic!("the over-ceiling result keeps its text")
        };
        assert!(text.text.starts_with("PNG 3x2\n[image omitted:"));

        let results = run_two_image_calls(2 * one).await;
        assert_eq!(results.iter().map(image_count).sum::<usize>(), 2);
    }
}
