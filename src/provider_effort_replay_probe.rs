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
    let capture = crate::agent::native_capture::NativeCapture::default();
    let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(inner))
        .with_native_capture(capture.clone());
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
    let (captured_turns, capture_unavailable) = capture.snapshot();
    assert_eq!(capture_unavailable, None);
    assert_eq!(captured_turns.len(), 4);
    assert!(
        captured_turns
            .iter()
            .all(|turn| turn.configured_model == "claude-opus-5-5")
    );
    let captured_native =
        rig::providers::anthropic::completion::Message::try_from(Message::Assistant {
            id: None,
            content: captured_turns[0].content.clone(),
        })
        .unwrap();
    let captured_native = serde_json::to_value(captured_native).unwrap();
    assert_eq!(captured_turns[0].advertised_tools.len(), 2);
    assert_eq!(
        captured_turns[0]
            .advertised_tools
            .iter()
            .map(|tool| tool.tool_id.as_str())
            .collect::<Vec<_>>(),
        ["toolu_probe_one", "toolu_probe_two"]
    );
    assert!(
        captured_turns[0]
            .advertised_tools
            .iter()
            .all(|tool| !tool.internal_id.is_empty() && tool.provider_id.is_none()),
        "native canonical tool ID must not be relabeled as an optional provider call ID"
    );
    assert_ne!(
        captured_turns[0].advertised_tools[0].internal_id,
        captured_turns[0].advertised_tools[1].internal_id
    );
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
        "owned_captured_turns":captured_turns,
        "captured_native_content":captured_native["content"],
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
            case["captured_native_content"], case["expected_first_assistant"],
            "owned capture changed ModelTurnFinished content"
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

#[test]
fn effort46_owned_capture_interactive_complete_recovered_and_incomplete_controls() {
    std::thread::Builder::new()
        .name("effort46-native-capture-one-mib".into())
        .stack_size(1_048_576)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(owned_capture_interactive_controls());
        })
        .unwrap()
        .join()
        .expect("native capture one-MiB consumer");
}

async fn owned_capture_interactive_controls() {
    use crate::agent::native_capture::{NativeCapture, Unavailable};
    use crate::event::AgentEvent;
    let incomplete = start()
        + &event(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        )
        + &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"partial display"}}),
        )
        + &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"incomplete signature"}}),
        );
    for (case, response) in [
        (
            "complete",
            start()
                + &thinking(0, "", "completed empty signature")
                + &redacted(1)
                + &text(2, "settled")
                + &finish(),
        ),
        (
            "unknown",
            tool_turn("", false).replace("probe_read", "unregistered_tool"),
        ),
        ("incomplete", incomplete),
    ] {
        let wire = Wire::new(vec![(true, response)]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let inner = rig::agent::AgentBuilder::new(model)
            .max_tokens(1000)
            .default_max_turns(1)
            .build();
        let capture = NativeCapture::default();
        let mut runner = AnyAgent::without_skills(AnyAgentInner::Anthropic(inner))
            .with_native_capture(capture.clone())
            .spawn_runner(
                "literal capture control".into(),
                Vec::<Message>::new(),
                RetryConfig {
                    max_attempts: 1,
                    ..Default::default()
                },
                #[cfg(feature = "hooks")]
                None,
            )
            .await;
        let mut done = false;
        let mut failed = false;
        let mut usage = crate::event::UsageDelta::default();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = runner.event_rx.recv().await {
                match event {
                    AgentEvent::Done { .. } => done = true,
                    AgentEvent::Error { .. } => failed = true,
                    AgentEvent::UsageDelta {
                        usage: delta,
                        awaits_compaction_decision,
                        ..
                    } => {
                        usage.input_tokens += delta.input_tokens;
                        usage.output_tokens += delta.output_tokens;
                        if awaits_compaction_decision {
                            runner
                                .compaction_decision_tx
                                .as_ref()
                                .unwrap()
                                .send(crate::agent::runner::CompactionBoundaryDecision::Continue)
                                .await
                                .unwrap();
                        }
                    }
                    AgentEvent::ToolCall { .. } | AgentEvent::ToolResult { .. } => {
                        panic!("zero registered tools dispatched in {case}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("owned interactive runner retirement");
        let (frames, unavailable) = capture.snapshot();
        match case {
            "complete" => {
                assert!(done && !failed);
                assert_eq!(unavailable, None);
                assert_eq!(frames.len(), 1);
                assert!(frames[0].advertised_tools.is_empty());
                let native =
                    rig::providers::anthropic::completion::Message::try_from(Message::Assistant {
                        id: None,
                        content: frames[0].content.clone(),
                    })
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(native).unwrap()["content"],
                    json!([
                        {"type":"thinking","thinking":"","signature":"completed empty signature"},
                        {"type":"redacted_thinking","data":"opaque-redacted-two"},
                        {"type":"text","text":"settled"},
                    ])
                );
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 2);
            }
            "unknown" => {
                assert!(failed && !done);
                assert_eq!(unavailable, Some(Unavailable::RecoveredToolContent));
                assert!(
                    frames.is_empty(),
                    "recovered model content became an accepted projection"
                );
            }
            "incomplete" => {
                assert!(
                    frames
                        .iter()
                        .all(|frame| frame.content.iter().all(|block| !matches!(
                            block,
                            rig::completion::AssistantContent::Reasoning(_)
                        ))),
                    "incomplete signature became completed content"
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(wire.finish().len(), 1);
    }
}

#[tokio::test]
async fn effort46_owned_capture_precedes_canonical_budget_stop_without_second_charge_or_effect() {
    use crate::agent::native_capture::NativeCapture;
    use crate::spend::{Account, Ledger, PriceSnapshot, RequestKind, run::Run};
    let root = crate::tests::OwnedTestDirectory::new("effort46-capture-budget");
    let ledger = Arc::new(Ledger::new(root.path()));
    let prices = PriceSnapshot::capture(Some((2.0, 4.0)), Default::default(), true);
    let profile = Run::fixture(
        Account::new("capture-budget", Some(0.000001), 0.0).unwrap(),
        ledger.clone(),
        "capture-budget",
        prices,
    );
    let profile = profile.with_captured_profile(
        "anthropic",
        "claude-opus-5-5",
        prices,
        &crate::config::Config::default(),
        RequestKind::Turn,
    );
    let wire = Wire::new(vec![(true, tool_turn("", true))]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = rig::agent::AgentBuilder::new(model)
        .max_tokens(1000)
        .default_max_turns(2)
        .tool(ProbeRead(calls.clone()))
        .build();
    let capture = NativeCapture::default();
    let turn = AnyAgent::without_skills(AnyAgentInner::Anthropic(inner))
        .with_native_capture(capture.clone())
        .with_spend(Some(profile.clone()))
        .run_print_with_output(
            "literal capped request",
            crate::print::HeadlessOutputPolicy::legacy(false, true),
            &RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            Vec::<Message>::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert_eq!(
        turn.failure.as_ref().unwrap().to_string(),
        crate::spend::run::BUDGET_STOP
    );
    assert_eq!(turn.usage.input_tokens, 10);
    assert_eq!(turn.usage.output_tokens, 2);
    assert_eq!(calls.load(Ordering::Acquire), 0);
    let (frames, unavailable) = capture.snapshot();
    assert_eq!(unavailable, None);
    assert_eq!(
        frames.len(),
        1,
        "later accounting termination suppressed accepted native content"
    );
    assert_eq!(
        frames[0]
            .content
            .iter()
            .filter(|item| matches!(item, rig::completion::AssistantContent::ToolCall(_)))
            .count(),
        2
    );
    assert_eq!(
        frames[0].advertised_tools.len(),
        2,
        "guard-skipped tool callbacks still require exact correlation"
    );
    assert!(
        frames[0]
            .advertised_tools
            .iter()
            .all(|tool| tool.provider_id.is_none())
    );
    let totals = ledger
        .summary_for_session("capture-budget")
        .unwrap()
        .session;
    assert_eq!(totals.records, 1);
    assert_eq!(totals.input_tokens, 10);
    assert_eq!(totals.output_tokens, 2);
    assert!((totals.known_cost - 0.000028).abs() < 1e-12);
    let account = profile.account.snapshot();
    assert!((account.recorded_spend() - 0.000028).abs() < 1e-12);
    assert!(!account.tracking_error);
    assert!(profile.admit().is_err());
    assert_eq!(wire.finish().len(), 1);
}

#[tokio::test]
async fn effort46_native_display_deltas_once_preserves_completed_opaque_content() {
    use crate::agent::native_capture::NativeCapture;
    use crate::event::AgentEvent;
    for enabled in [false, true] {
        for summary in ["", "visible native summary"] {
            let response = start()
                + &thinking(0, summary, "display-control-signature")
                + &redacted(1)
                + &text(2, "settled")
                + &finish();
            let wire = Wire::new(vec![(true, response)]);
            let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5")
            else {
                unreachable!()
            };
            let inner = rig::agent::AgentBuilder::new(model)
                .max_tokens(1000)
                .default_max_turns(1)
                .build();
            let capture = NativeCapture::default();
            let mut agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(inner));
            if enabled {
                agent = agent.with_native_capture(capture.clone());
            }
            let mut runner = agent
                .spawn_runner(
                    "display and opaque retention control".into(),
                    Vec::<Message>::new(),
                    RetryConfig {
                        max_attempts: 1,
                        ..Default::default()
                    },
                    #[cfg(feature = "hooks")]
                    None,
                )
                .await;
            let mut displayed = String::new();
            let mut completed = None;
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(event) = runner.event_rx.recv().await {
                    match event {
                        AgentEvent::Reasoning(text) => displayed.push_str(&text),
                        AgentEvent::Done { interactions, .. } => completed = Some(interactions),
                        AgentEvent::Error { message, .. } => {
                            panic!("native display control failed: {message}")
                        }
                        AgentEvent::UsageDelta {
                            awaits_compaction_decision: true,
                            ..
                        } => {
                            runner
                                .compaction_decision_tx
                                .as_ref()
                                .unwrap()
                                .send(crate::agent::runner::CompactionBoundaryDecision::Continue)
                                .await
                                .unwrap();
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("owned native display runner retirement");
            assert_eq!(
                displayed, summary,
                "native summary was duplicated or opaque content rendered"
            );
            assert!(!displayed.contains("display-control-signature"));
            assert!(!displayed.contains("opaque-redacted-two"));
            let transcript =
                serde_json::to_string(&completed.expect("successful terminal interactions"))
                    .unwrap();
            assert!(transcript.contains("display-control-signature"));
            assert!(transcript.contains("opaque-redacted-two"));
            if enabled {
                let (frames, unavailable) = capture.snapshot();
                assert_eq!(unavailable, None);
                assert_eq!(frames.len(), 1);
                let native =
                    rig::providers::anthropic::completion::Message::try_from(Message::Assistant {
                        id: None,
                        content: frames[0].content.clone(),
                    })
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(native).unwrap()["content"],
                    json!([
                        {"type":"thinking", "thinking":summary, "signature":"display-control-signature"},
                        {"type":"redacted_thinking", "data":"opaque-redacted-two"},
                        {"type":"text", "text":"settled"},
                    ])
                );
            } else {
                assert!(capture.snapshot().0.is_empty());
            }
            assert_eq!(wire.finish().len(), 1);
        }
    }
}

#[tokio::test]
async fn effort46_malformed_native_sse_never_reports_opaque_signature_or_redacted_bytes() {
    for payload in [
        json!({"type":"content_block_delta","index":"opaque-signature-index-sentinel",
            "delta":{"type":"signature_delta","signature":"opaque-signature-value-sentinel"}}),
        json!({"type":"content_block_start","index":"opaque-redacted-index-sentinel",
            "content_block":{"type":"redacted_thinking","data":"opaque-redacted-value-sentinel"}}),
        json!({"type":{"private":"opaque-private-value-sentinel"}}),
    ] {
        let wire = Wire::new(vec![(true, start() + &event("malformed", payload))]);
        let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
            "claude-opus-5-5",
            crate::config::PromptCacheTtl::default(),
        ) else {
            unreachable!()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let capture = crate::agent::native_capture::NativeCapture::default();
        let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(
            rig::agent::AgentBuilder::new(model)
                .preamble("Stable malformed-event control")
                .max_tokens(1000)
                .tool(ProbeRead(calls.clone()))
                .build(),
        ))
        .with_native_capture(capture.clone());
        let outcome = agent
            .run_print_with_output(
                "literal malformed-event control",
                crate::print::HeadlessOutputPolicy::legacy(false, true),
                &RetryConfig {
                    max_attempts: 1,
                    ..Default::default()
                },
                Vec::<Message>::new(),
                #[cfg(feature = "hooks")]
                None,
            )
            .await;
        let error = outcome.failure.expect("actual malformed stream must fail");
        let rendered = format!("{error}\n{error:?}");
        assert!(
            rendered.contains("Invalid Anthropic streaming event"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("opaque-"),
            "native diagnostic disclosed opaque input: {rendered}"
        );
        assert!(
            capture.snapshot().0.is_empty(),
            "malformed native data was accepted"
        );
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert_eq!(wire.finish().len(), 1);
    }
}
