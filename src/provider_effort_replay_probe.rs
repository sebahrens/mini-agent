//! Native completed-content projection probes; saved-history repair is separate.

use super::*;
use std::sync::atomic::AtomicUsize;

#[derive(serde::Deserialize)]
struct ReadArgs {
    step: u64,
}

struct ProbeRead(Arc<AtomicUsize>);

impl rig::tool::Tool for ProbeRead {
    const NAME: &'static str = "probe_read";
    type Args = ReadArgs;
    type Output = String;
    type Error = std::io::Error;

    fn description(&self) -> String {
        "Read a fixed inert test value; no external effects".into()
    }

    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"step":{"type":"integer"}},"required":["step"],"additionalProperties":false})
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(format!("inert read {}", args.step))
    }
}

fn tool(index: usize, id: &str, step: u64) -> String {
    event(
        "content_block_start",
        json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":"probe_read","input":{}}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":json!({"step":step}).to_string()}}),
    ) + &event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":index}),
    )
}

fn thinking(index: usize, display: &str, signature: &str) -> String {
    event(
        "content_block_start",
        json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":"","signature":""}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":display}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":signature}}),
    ) + &event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":index}),
    )
}

fn redacted(index: usize) -> String {
    event(
        "content_block_start",
        json!({"type":"content_block_start","index":index,"content_block":{"type":"redacted_thinking","data":"opaque-redacted-two"}}),
    ) + &event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":index}),
    )
}

fn tool_turn(summary: &str, canonical: bool) -> String {
    let blocks = if canonical {
        thinking(0, "", "opaque-signature-one")
            + &redacted(1)
            + &text(2, "TextA")
            + &tool(3, "toolu_probe_one", 1)
            + &text(4, "TextB")
            + &tool(5, "toolu_probe_two", 2)
    } else {
        text(0, "TextA")
            + &thinking(1, "", "opaque-signature-one")
            + &tool(2, "toolu_probe_one", 1)
            + &text(3, "TextB")
            + &thinking(4, summary, "opaque-signature-two")
            + &tool(5, "toolu_probe_two", 2)
    };
    start()
        + &blocks
        + &event(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":2}}),
        )
        + &event("message_stop", json!({"type":"message_stop"}))
}

fn without_cache(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("cache_control");
            for child in map.values_mut() {
                without_cache(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                without_cache(child);
            }
        }
        _ => {}
    }
}

fn first_assistant(request: &Value) -> Value {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap()["content"]
        .clone()
}

async fn run_probe(summary: &str, canonical: bool) -> Value {
    // Observe the actual generic low-level choice before the agent assembler.
    // This consumes one separate fixture response; it never dispatches a tool.
    let raw_wire = Wire::new(vec![(true, tool_turn(summary, canonical))]);
    let AnyModel::Anthropic(raw_model) = raw_wire.client().completion_model("claude-opus-5-5")
    else {
        unreachable!()
    };
    let mut raw_stream = raw_model
        .completion_request("raw typed capture only")
        .max_tokens(1000)
        .stream()
        .await
        .unwrap();
    while let Some(item) = raw_stream.next().await {
        item.unwrap();
    }
    let raw_choice = raw_stream.choice.clone();
    let raw_native = rig::providers::anthropic::completion::Message::try_from(Message::Assistant {
        id: None,
        content: raw_choice.clone(),
    })
    .unwrap();
    let raw_native = serde_json::to_value(raw_native).unwrap();
    let raw_request = raw_wire.finish();
    assert_eq!(raw_request.len(), 1);
    let response = start() + &text(0, "settled") + &finish();
    let wire = Wire::new(vec![
        (true, tool_turn(summary, canonical)),
        (true, response.clone()),
        (true, response.clone()),
        (true, response),
    ]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = rig::agent::AgentBuilder::new(model)
        .preamble("Stable native probe system")
        .max_tokens(1000)
        .default_max_turns(2)
        .tool(ProbeRead(Arc::clone(&calls)))
        .build();
    let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(inner));
    let output = crate::print::HeadlessOutputPolicy::legacy(false, true);
    let retry = RetryConfig {
        max_attempts: 1,
        ..Default::default()
    };
    let turn = agent
        .run_print_with_output(
            "literal initial user",
            output,
            &retry,
            Vec::<Message>::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(turn.failure.is_none(), "{:?}", turn.failure);
    assert_eq!(calls.load(Ordering::Acquire), 2);
    assert_eq!(turn.usage.input_tokens, 20);
    assert_eq!(turn.usage.output_tokens, 4);

    let mut session =
        crate::session::Session::new("effort-probe", "claude-opus-5-5", 200_000, "probe");
    crate::print::persist_headless_turn(
        &mut session,
        "literal initial user",
        &turn.response,
        &turn.interactions,
    );
    session.charge_usage_delta(turn.usage.into(), true);
    let history = crate::agent::runner::convert_history(&session);
    let first_replay = agent
        .run_print_with_output(
            "literal followup user",
            output,
            &retry,
            history,
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(first_replay.failure.is_none());
    assert_eq!(calls.load(Ordering::Acquire), 2);

    // Use the production private atomic session writer and the same serde
    // codec, without installing process-global artifact roots or lock state.
    let directory = crate::tests::OwnedTestDirectory::new("effort46-replay-codec");
    let path = directory.path().join("session.json");
    crate::session::storage::atomic_write(&path, &serde_json::to_string(&session).unwrap())
        .unwrap();
    let mut file =
        crate::fs::open_private_file_checked(&path, crate::fs::PrivateFileAccess::Read).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    let reloaded: crate::session::Session = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(reloaded.total_input_tokens, 20);
    assert_eq!(reloaded.total_output_tokens, 4);
    let second_replay = agent
        .run_print_with_output(
            "literal followup user",
            output,
            &retry,
            crate::agent::runner::convert_history(&reloaded),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(second_replay.failure.is_none());
    assert_eq!(calls.load(Ordering::Acquire), 2);
    let mut requests = wire.finish();
    assert_eq!(requests.len(), 4);
    requests.iter_mut().for_each(without_cache);
    let interleaved_expected = json!([
        {"type":"text","text":"TextA"},
        {"type":"thinking","thinking":"","signature":"opaque-signature-one"},
        {"type":"tool_use","id":"toolu_probe_one","name":"probe_read","input":{"step":1}},
        {"type":"text","text":"TextB"},
        {"type":"thinking","thinking":summary,"signature":"opaque-signature-two"},
        {"type":"tool_use","id":"toolu_probe_two","name":"probe_read","input":{"step":2}}
    ]);
    let expected = if canonical {
        json!([
            {"type":"thinking","thinking":"","signature":"opaque-signature-one"},
            {"type":"redacted_thinking","data":"opaque-redacted-two"},
            {"type":"text","text":"TextA"},
            {"type":"tool_use","id":"toolu_probe_one","name":"probe_read","input":{"step":1}},
            {"type":"text","text":"TextB"},
            {"type":"tool_use","id":"toolu_probe_two","name":"probe_read","input":{"step":2}}
        ])
    } else {
        interleaved_expected
    };
    let original_typed = expected
        .as_array()
        .unwrap()
        .iter()
        .map(|block| {
            let native: rig::providers::anthropic::completion::Content =
                serde_json::from_value(block.clone()).unwrap();
            rig::message::AssistantContent::try_from(native).unwrap()
        })
        .collect::<Vec<_>>();
    let original_message = Message::Assistant {
        id: None,
        content: rig::OneOrMany::many(original_typed.clone()).unwrap(),
    };
    let native_direct =
        rig::providers::anthropic::completion::Message::try_from(original_message).unwrap();
    let native_direct = serde_json::to_value(native_direct).unwrap();
    assert_eq!(
        native_direct["content"], expected,
        "native converter must preserve supplied typed order"
    );
    json!({
        "sequence_case":if canonical{"thinking_first_with_redacted"}else{"adversarial_interleaved"},
        "original_typed_assistant":original_typed,
        "raw_sdk_choice_before_assembler":raw_choice,
        "raw_choice_native_conversion":raw_native,
        "raw_capture_request":raw_request[0],
        "native_direct_conversion":native_direct,
        "runner_observable_interactions":turn.interactions,
        "persisted_messages":session.messages,
        "display_case":if summary.is_empty(){"omitted"}else{"summarized"},
        "requests":requests,
        "expected_first_assistant":expected,
        "first_sdk_followup":first_assistant(&requests[1]),
        "converted_live_followup":first_assistant(&requests[2]),
        "converted_reloaded_followup":first_assistant(&requests[3]),
        "observed_input":20,"observed_output":4,"actual_tool_calls":2,
        "persistence_scope":"production private atomic writer plus Session serde codec; global save/claim API intentionally not installed"
    })
}

#[tokio::test]
async fn effort46_native_completed_projection_preserves_sdk_followup_order() {
    let mut cases = Vec::new();
    for summary in ["", "Readable summary"] {
        cases.push(run_probe(summary, false).await);
    }
    cases.push(run_probe("", true).await);
    println!("EFFORT46_SDK_PROJECTION_CAPTURE={}", json!({"cases":cases}));
    for case in cases {
        assert_eq!(
            case["raw_choice_native_conversion"]["content"], case["expected_first_assistant"],
            "native completed projection changed or duplicated blocks"
        );
        assert_eq!(
            case["first_sdk_followup"], case["expected_first_assistant"],
            "SDK native continuation changed original assistant sequence"
        );
    }
}

#[derive(Clone)]
struct GenericDeltaResponse;

impl rig::completion::GetTokenUsage for GenericDeltaResponse {
    fn token_usage(&self) -> rig::completion::Usage {
        rig::completion::Usage::new()
    }
}

#[tokio::test]
async fn effort46_generic_delta_only_reasoning_aggregation_remains_enabled() {
    use rig::streaming::{RawStreamingChoice, StreamingCompletionResponse, StreamingResult};
    let items: StreamingResult<GenericDeltaResponse> = Box::pin(futures::stream::iter([
        Ok(RawStreamingChoice::ReasoningDelta {
            id: Some("generic_reasoning".into()),
            reasoning: "generic delta".into(),
        }),
        Ok(RawStreamingChoice::Message("answer".into())),
        Ok(RawStreamingChoice::FinalResponse(GenericDeltaResponse)),
    ]));
    let mut stream = StreamingCompletionResponse::stream(items);
    while let Some(item) = stream.next().await {
        item.unwrap();
    }
    let reasoning = stream
        .choice
        .iter()
        .filter_map(|item| match item {
            rig::message::AssistantContent::Reasoning(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reasoning.len(), 1);
    assert_eq!(reasoning[0].id.as_deref(), Some("generic_reasoning"));
    assert!(
        matches!(reasoning[0].content.as_slice(),[rig::message::ReasoningContent::Text{text,signature:None}] if text=="generic delta")
    );
    assert!(stream.choice.iter().any(
        |item| matches!(item,rig::message::AssistantContent::Text(text) if text.text=="answer")
    ));
}

#[tokio::test]
async fn effort46_native_interrupted_nonempty_thinking_is_display_only() {
    let partial = event(
        "content_block_start",
        json!({"type":"content_block_start","index":0,
        "content_block":{"type":"thinking","thinking":"","signature":""}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,
            "delta":{"type":"thinking_delta","thinking":"partial display"}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,
            "delta":{"type":"signature_delta","signature":"incomplete signature"}}),
    );
    let wire = Wire::new(vec![(true, start() + &partial)]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let mut stream = model
        .completion_request("interrupted native block")
        .max_tokens(1000)
        .stream()
        .await
        .unwrap();
    let mut displayed = String::new();
    while let Some(item) = stream.next().await {
        if let rig::streaming::StreamedAssistantContent::ReasoningDelta { reasoning, .. } =
            item.unwrap()
        {
            displayed.push_str(&reasoning);
        }
    }
    assert_eq!(displayed, "partial display");
    assert!(
        !stream
            .choice
            .iter()
            .any(|item| matches!(item, rig::message::AssistantContent::Reasoning(_))),
        "incomplete native thinking became completed replay content"
    );
    assert_eq!(wire.finish().len(), 1);
}
