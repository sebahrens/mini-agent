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

#[test]
fn effort46_shared_binding_and_denial_on_one_mib() {
    std::thread::Builder::new()
        .name("native-shared-binding-one-mib".into())
        .stack_size(1_048_576)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    native_bound_interactive_turn().await;
                    crate::startup::heap_future(native_retired_outcome_controls).await;
                    crate::startup::heap_future(native_interactive_zero_visible_controls).await;
                    crate::startup::heap_future(native_question_retired_outcome_control).await;
                    crate::startup::heap_future(|| native_refused_checkpoint_controls(true)).await;
                    crate::startup::heap_future(|| native_refused_checkpoint_controls(false)).await;
                })
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn native_bound_interactive_turn() {
    use crate::event::AgentEvent;
    use crate::session::{
        MessageRole,
        native_replay::{HistoryProtocol, ReplayBoundary, RestartReason, RetiredNativeOutcome},
    };
    let wire = Wire::new(vec![
        (true, tool_turn("", true)),
        (true, start() + &text(0, "settled") + &finish()),
    ]);
    let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
        "claude-opus-5-5",
        crate::config::PromptCacheTtl::default(),
    ) else {
        unreachable!()
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let mut session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    session.add_message(MessageRole::User, "prior original user bytes");
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Stable bound UI system")
            .max_tokens(1000)
            .default_max_turns(2)
            .tool(ProbeRead(calls.clone()))
            .build(),
    ))
    .bind_native_turn(
        &session,
        vec![Message::user("prior original user bytes")].into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Lifecycle,
    )
    .unwrap();
    session.add_message(MessageRole::User, "literal live user");
    let mut runner = crate::startup::heap_future(|| {
        agent.spawn_runner(
            "literal live user".into(),
            binding.history(),
            RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            #[cfg(feature = "hooks")]
            None,
        )
    })
    .await;
    let mut response = String::new();
    let mut completed = false;
    let mut lifecycle_ids = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                AgentEvent::Token(text) => response.push_str(&text),
                AgentEvent::ToolCall { id, name, args } => {
                    if !response.is_empty() {
                        session.add_message(MessageRole::Assistant, &response);
                    }
                    response.clear();
                    lifecycle_ids.push(id.clone());
                    session.add_tool_call_with_id(&id, &name, &args);
                }
                AgentEvent::ToolResult {
                    id, name, output, ..
                } => {
                    session.add_tool_result_with_id_and_artifact(&id, &name, &output);
                }
                AgentEvent::Done { response, .. } => {
                    session.add_message(MessageRole::Assistant, &response);
                    completed = true;
                }
                AgentEvent::Error { message, .. } => panic!("bound live turn failed: {message}"),
                AgentEvent::UsageDelta {
                    usage,
                    awaits_compaction_decision,
                    ..
                } => {
                    session.charge_usage_delta(usage, true);
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
                _ => {}
            }
        }
    })
    .await
    .expect("exact bound interactive runner retirement");
    assert!(completed);
    assert_eq!(calls.load(Ordering::Acquire), 2);
    assert_eq!(lifecycle_ids.len(), 2);
    assert_eq!(session.total_input_tokens, 20);
    assert_eq!(session.total_output_tokens, 4);
    assert!(binding.accepted_progress());
    assert_eq!(binding.refusal(), None);
    // Independent negative candidates reuse only the exact already-retired
    // SDK observations. This is a test fixture, not a production Clone API or
    // a second actor/effect owner. Each candidate gets one retirement attempt.
    let candidate = || crate::provider::NativeTurnBinding {
        session_id: binding.session_id.clone(),
        epoch: binding.epoch,
        message_start: binding.message_start,
        preappend: binding.preappend,
        compactions: binding.compactions,
        previous_frames: binding.previous_frames,
        recorded_tool_ids: binding.recorded_tool_ids,
        retirement_claimed: std::sync::atomic::AtomicBool::new(false),
        host: binding.host.clone(),
        history: binding.history.clone(),
        capture: binding.capture.clone(),
    };
    let mut changed_compaction = session.clone();
    changed_compaction.native_frontier = session.native_frontier.fork();
    changed_compaction
        .compactions
        .push(crate::session::Compaction {
            file_requests: Default::default(),
            summary: "changed bound context".into(),
            first_kept_index: 1,
            summarized_count: 1,
            token_savings: 10,
            created_at: "2026-10-14T00:00:00Z".into(),
            memory_ref: None,
        });
    assert_eq!(
        candidate()
            .settle_into_with_outcome(&mut changed_compaction, RetiredNativeOutcome::Completed),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(
        changed_compaction.native_frontier.refusal(),
        Some(RestartReason::ChangedPrefix)
    );
    let mut changed_frames = session.clone();
    changed_frames.native_frontier = session.native_frontier.fork();
    changed_frames
        .store_native_history(
            binding
                .capture
                .as_ref()
                .unwrap()
                .persisted_history()
                .unwrap()
                .with_retired_outcome(RetiredNativeOutcome::Completed)
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        candidate().settle_into_with_outcome(&mut changed_frames, RetiredNativeOutcome::Completed),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(
        changed_frames.native_frontier.refusal(),
        Some(RestartReason::ChangedPrefix)
    );
    let mut foreign = session.clone();
    foreign.native_frontier = session.native_frontier.fork();
    foreign.id = "foreign logical session".into();
    assert_eq!(
        candidate().settle_into_with_outcome(&mut foreign, RetiredNativeOutcome::Completed),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(foreign.native_frontier.refusal(), None);
    let mut newer = session.clone();
    newer.native_frontier = session.native_frontier.fork();
    newer.native_frontier.retire().unwrap();
    assert_eq!(
        candidate().settle_into_with_outcome(&mut newer, RetiredNativeOutcome::Completed),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(newer.native_frontier.refusal(), None);
    let mut edited_prefix = session.clone();
    edited_prefix.native_frontier = session.native_frontier.fork();
    edited_prefix.messages[0].content = "same-length edited prior user".into();
    assert_eq!(
        candidate().settle_into_with_outcome(&mut edited_prefix, RetiredNativeOutcome::Completed),
        Err(RestartReason::ChangedPrefix),
        "same row count is not the bound preappend content"
    );
    assert_eq!(
        edited_prefix.native_frontier.refusal(),
        Some(RestartReason::ChangedPrefix)
    );
    let mut missing = session.clone();
    missing.native_frontier = session.native_frontier.fork();
    missing
        .messages
        .retain(|row| row.role != MessageRole::ToolCall);
    assert_eq!(
        candidate().settle_into_with_outcome(&mut missing, RetiredNativeOutcome::Completed),
        Err(RestartReason::UnavailableSource),
        "all advertised calls must have exact persisted call rows"
    );
    assert_eq!(
        missing.native_frontier.refusal(),
        Some(RestartReason::UnavailableSource)
    );
    let mut wrong = session.clone();
    wrong.native_frontier = session.native_frontier.fork();
    let older_live_clone = wrong.clone();
    let row = wrong
        .messages
        .iter_mut()
        .find(|row| row.role == MessageRole::ToolCall)
        .unwrap();
    row.tool_call_id = Some("unadvertised-lifecycle-id".into());
    let before_feed = serde_json::to_value(&wrong.messages).unwrap();
    assert_eq!(
        candidate().settle_into_with_outcome(&mut wrong, RetiredNativeOutcome::Completed),
        Err(RestartReason::UnavailableSource)
    );
    assert_eq!(serde_json::to_value(&wrong.messages).unwrap(), before_feed);
    assert_eq!(wrong.native_frontier.current(), 1);
    assert_eq!(
        wrong.native_frontier.refusal(),
        Some(RestartReason::UnavailableSource)
    );
    assert!(!older_live_clone.native_frontier.ready());
    // Even an older clone serializes the shared denial, not a healthy stale
    // cutoff that could auto-restart after reload.
    for denied in [&wrong, &older_live_clone] {
        let json = serde_json::to_string(denied).unwrap();
        let imported = crate::session::storage::decode_session_json(&json).unwrap();
        assert_eq!(
            imported.native_frontier.refusal(),
            Some(RestartReason::UnavailableSource)
        );
        assert!(matches!(
            imported.native_restore_history(
                HistoryProtocol::AnthropicMessages,
                ReplayBoundary::CompletedTurn
            ),
            Err(RestartReason::UnavailableSource),
        ));
    }
    let forked = wrong.native_frontier.fork();
    assert_eq!(forked.refusal(), Some(RestartReason::UnavailableSource));
    assert!(!forked.ready());
    let mut no_outcome = session.clone();
    no_outcome.native_frontier = session.native_frontier.fork();
    assert_eq!(
        candidate().settle_into(&mut no_outcome),
        Err(RestartReason::IncompleteTurn)
    );
    assert_eq!(
        no_outcome.native_frontier.refusal(),
        Some(RestartReason::IncompleteTurn)
    );
    assert_eq!(
        binding.settle_into_with_outcome(&mut session, RetiredNativeOutcome::Completed),
        Ok(true)
    );
    let settled_once = serde_json::to_value(&session).unwrap();
    assert_eq!(
        binding.settle_into_with_outcome(&mut session, RetiredNativeOutcome::Completed),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(
        serde_json::to_value(&session).unwrap(),
        settled_once,
        "duplicate retired settlement must not fence its own attached chain"
    );
    let frames = session.native_history_snapshot().unwrap().frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].native_tool_results.len(), 2);
    assert!(frames[1].native_input_delta.as_ref().unwrap().iter().all(|message|
        !matches!(message, Message::User { content } if content.iter().any(|item| matches!(item, rig::message::UserContent::ToolResult(_))))),
        "prepared delta duplicated already-captured grouped results");
    for id in &lifecycle_ids {
        assert_eq!(
            frames
                .iter()
                .flat_map(|frame| &frame.advertised_tools)
                .filter(|identity| identity.internal_id == id.as_str())
                .count(),
            1
        );
    }
    let encoded = serde_json::to_string(&session).unwrap();
    for location in ["base", "delta"] {
        let mut extra_opaque = serde_json::to_value(&session).unwrap();
        let untracked = serde_json::to_value(Message::Assistant {
            id: None,
            content: rig::OneOrMany::one(rig::completion::AssistantContent::Reasoning(
                rig::message::Reasoning::new_with_signature(
                    "",
                    Some("untracked imported signature".into()),
                ),
            )),
        })
        .unwrap();
        if location == "base" {
            extra_opaque["native_history"]["base"]
                .as_array_mut()
                .unwrap()
                .push(untracked);
        } else {
            extra_opaque["native_history"]["frames"][1]["native_input_delta"]
                .as_array_mut()
                .unwrap()
                .push(untracked);
        }
        assert!(
            crate::session::storage::decode_session_json(
                &serde_json::to_string(&extra_opaque).unwrap()
            )
            .is_err(),
            "untracked imported opaque {location} must remain unavailable"
        );
    }
    let mut missing_witness = serde_json::to_value(&session).unwrap();
    missing_witness["native_history"]["frames"][0]["native_commitment"]["blocks"] = json!([]);
    let tampered: crate::session::Session =
        serde_json::from_value(missing_witness.clone()).unwrap();
    assert!(
        crate::session::storage::decode_session_json(
            &serde_json::to_string(&missing_witness).unwrap()
        )
        .is_err(),
        "signed/redacted content cannot import without its complete witness"
    );
    let negative = Wire::new(Vec::new());
    let AnyModel::Anthropic(model) = negative.client().completion_model_with_cache_ttl(
        "claude-opus-5-5",
        crate::config::PromptCacheTtl::default(),
    ) else {
        unreachable!()
    };
    let rejected = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Changed system after import")
            .max_tokens(1000)
            .build(),
    ))
    .bind_native_turn(
        &tampered,
        Vec::<Message>::new().into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Lifecycle,
    );
    assert!(matches!(rejected, Err(RestartReason::UnavailableSource)));
    assert!(
        negative.finish().is_empty(),
        "malformed witness reached a live native endpoint"
    );
    let restored = crate::session::storage::decode_session_json(&encoded).unwrap();
    let history = restored
        .native_restore_history(
            HistoryProtocol::AnthropicMessages,
            ReplayBoundary::CompletedTurn,
        )
        .unwrap();
    let first = rig::providers::anthropic::completion::Message::try_from(
        history
            .iter()
            .find(|message| matches!(message, Message::Assistant { .. }))
            .unwrap()
            .clone(),
    )
    .unwrap();
    let bodies = wire.finish();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        serde_json::to_value(first).unwrap()["content"],
        first_assistant(&bodies[1])
    );
    assert!(session.permission_allowlist.is_empty());
}

async fn native_interactive_zero_visible_controls() {
    use crate::event::AgentEvent;
    use crate::session::native_replay::{ReplayBoundary, RestartReason, RetiredNativeOutcome};
    for (redacted_only, end_turn) in [(false, true), (true, true), (false, false)] {
        let blocks = if redacted_only {
            redacted(0)
        } else {
            thinking(0, "", "interactive signed-only")
        };
        let response = start()
            + &blocks
            + &event(
                "message_delta",
                json!({"type":"message_delta", "delta":{"stop_reason": if end_turn {"end_turn"} else {"max_tokens"}, "stop_sequence":null}, "usage":{"output_tokens":2}}),
            )
            + &event("message_stop", json!({"type":"message_stop"}));
        let wire = Wire::new(vec![(true, response)]);
        let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
            "claude-opus-5-5",
            crate::config::PromptCacheTtl::default(),
        ) else {
            unreachable!()
        };
        let mut session =
            crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
        session.set_input_output_prices(Some((2.0, 4.0)));
        let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
            rig::agent::AgentBuilder::new(model)
                .preamble("Actual zero-visible interactive lifecycle")
                .max_tokens(1000)
                .default_max_turns(1)
                .build(),
        ))
        .bind_native_turn(
            &session,
            Vec::<Message>::new().into(),
            ReplayBoundary::CompletedTurn,
            crate::provider::RecordedToolIds::Lifecycle,
        )
        .unwrap();
        session.add_message(
            crate::session::MessageRole::User,
            "literal zero-visible actor",
        );
        let scope = crate::agent::runner::AgentWorkScope::new();
        let paused = crate::startup::heap_future(|| {
            agent.spawn_runner_paused_in_scope(
                "literal zero-visible actor".into(),
                binding.history(),
                RetryConfig {
                    max_attempts: 1,
                    ..Default::default()
                },
                #[cfg(feature = "hooks")]
                None,
                scope.clone(),
            )
        })
        .await;
        let mut runner = paused.start_interactive();
        let mut terminal = None;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = runner.event_rx.recv().await {
                match event {
                    AgentEvent::UsageDelta {
                        usage,
                        awaits_compaction_decision,
                        ..
                    } => {
                        session.charge_usage_delta(usage, true);
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
                    AgentEvent::Done { .. } => terminal = Some(RetiredNativeOutcome::Completed),
                    AgentEvent::Error { .. } => terminal = Some(RetiredNativeOutcome::Incomplete),
                    AgentEvent::Token(text) => {
                        assert!(text.is_empty(), "no fabricated answer text")
                    }
                    AgentEvent::Reasoning(text) => {
                        assert!(text.is_empty(), "opaque bytes never become display text")
                    }
                    AgentEvent::ToolCall { .. }
                    | AgentEvent::ToolResult { .. }
                    | AgentEvent::CompactionBoundary { .. }
                    | AgentEvent::AwaitingUser { .. } => panic!("unexpected tool or parked actor"),
                    _ => {}
                }
            }
            scope.wait_idle().await;
        })
        .await
        .expect("exact zero-visible actor retirement");
        assert_eq!(scope.active_children(), 0);
        assert!(runner.abort_handle.is_finished());
        assert_eq!(session.total_input_tokens, 10);
        assert_eq!(session.total_output_tokens, 2);
        assert!((session.total_cost - 0.000028).abs() < 1e-12);
        let outcome = terminal.expect("actual Done or Error from retired actor");
        assert_eq!(
            outcome,
            if end_turn {
                RetiredNativeOutcome::Completed
            } else {
                RetiredNativeOutcome::Incomplete
            }
        );
        let settled = binding.settle_into_with_outcome(&mut session, outcome);
        if end_turn {
            assert_eq!(settled, Ok(true));
            let encoded = serde_json::to_string(&session).unwrap();
            let restored = crate::session::storage::decode_session_json(&encoded).unwrap();
            assert_eq!(
                restored.native_history_snapshot().unwrap().saved_boundary(),
                Ok(ReplayBoundary::CompletedTurn)
            );
        } else {
            assert_eq!(settled, Err(RestartReason::IncompleteTurn));
            assert_eq!(
                session.native_frontier.refusal(),
                Some(RestartReason::IncompleteTurn)
            );
            assert!(session.native_history_snapshot().is_none());
        }
        assert!(session.permission_allowlist.is_empty());
        assert_eq!(wire.finish().len(), 1);
    }
}

async fn native_retired_outcome_controls() {
    use crate::event::AgentEvent;
    use crate::session::native_replay::{ReplayBoundary, RestartReason, RetiredNativeOutcome};
    // Actual successful headless retirement can have no display text at all.
    let wire = Wire::new(vec![(
        true,
        start() + &thinking(0, "", "signed-only completed") + &redacted(1) + &finish(),
    )]);
    let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
        "claude-opus-5-5",
        crate::config::PromptCacheTtl::default(),
    ) else {
        unreachable!()
    };
    let mut session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    session.set_input_output_prices(Some((2.0, 4.0)));
    let prices =
        crate::print::HeadlessPricing::from_session(&session, &crate::config::Config::default());
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Signed-only actor control")
            .max_tokens(1000)
            .default_max_turns(1)
            .build(),
    ))
    .bind_native_turn(
        &session,
        Vec::<Message>::new().into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Canonical,
    )
    .unwrap();
    let retry = RetryConfig {
        max_attempts: 1,
        ..Default::default()
    };
    let turn = crate::startup::heap_future(|| {
        agent.run_print_with_output(
            "literal signed-only",
            crate::print::HeadlessOutputPolicy::legacy(false, true),
            &retry,
            binding.history(),
            #[cfg(feature = "hooks")]
            None,
        )
    })
    .await;
    assert!(
        turn.failure.is_none(),
        "signed-only actor failed: {:?}",
        turn.failure
    );
    assert!(turn.response.is_empty());
    assert!(binding.accepted_progress());
    assert_eq!(
        binding.commit_canonical_into_with_outcome(
            &mut session,
            "literal signed-only",
            "",
            &turn.interactions,
            turn.usage.into(),
            prices,
            RetiredNativeOutcome::Completed
        ),
        Ok(true)
    );
    let restored =
        crate::session::storage::decode_session_json(&serde_json::to_string(&session).unwrap())
            .unwrap();
    let native = restored.native_history_snapshot().unwrap();
    assert_eq!(native.saved_boundary(), Ok(ReplayBoundary::CompletedTurn));
    assert!(native.require_completed_boundary().is_ok());
    let replay = restored
        .native_restore_history(
            crate::session::native_replay::HistoryProtocol::AnthropicMessages,
            ReplayBoundary::CompletedTurn,
        )
        .unwrap();
    let bytes = serde_json::to_string(&replay).unwrap();
    assert!(bytes.contains("signed-only completed"));
    assert!(bytes.contains("opaque-redacted-two"));
    assert_eq!(wire.finish().len(), 1);

    for park in [true, false] {
        let wire = Wire::new(vec![(true, tool_turn("", true))]);
        let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
            "claude-opus-5-5",
            crate::config::PromptCacheTtl::default(),
        ) else {
            unreachable!()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let mut session =
            crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
        session.set_input_output_prices(Some((2.0, 4.0)));
        let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
            rig::agent::AgentBuilder::new(model)
                .preamble("Actual pressure/error lifecycle control")
                .max_tokens(1000)
                .default_max_turns(if park { 2 } else { 1 })
                .tool(ProbeRead(calls.clone()))
                .build(),
        ))
        .bind_native_turn(
            &session,
            Vec::<Message>::new().into(),
            ReplayBoundary::CompletedTurn,
            crate::provider::RecordedToolIds::Lifecycle,
        )
        .unwrap();
        session.add_message(crate::session::MessageRole::User, "literal actor outcome");
        let scope = crate::agent::runner::AgentWorkScope::new();
        let paused = crate::startup::heap_future(|| {
            agent.spawn_runner_paused_in_scope(
                "literal actor outcome".into(),
                binding.history(),
                RetryConfig {
                    max_attempts: 1,
                    ..Default::default()
                },
                #[cfg(feature = "hooks")]
                None,
                scope.clone(),
            )
        })
        .await;
        let mut runner = paused.start_interactive();
        let mut terminal = None;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = runner.event_rx.recv().await {
                match event {
                    AgentEvent::ToolCall { id, name, args } => {
                        session.add_tool_call_with_id(&id, &name, &args)
                    }
                    AgentEvent::ToolResult {
                        id, name, output, ..
                    } => {
                        session.add_tool_result_with_id_and_artifact(&id, &name, &output);
                    }
                    AgentEvent::UsageDelta {
                        usage,
                        awaits_compaction_decision,
                        ..
                    } => {
                        session.charge_usage_delta(usage, true);
                        if awaits_compaction_decision {
                            runner
                                .compaction_decision_tx
                                .as_ref()
                                .unwrap()
                                .send(if park {
                                    crate::agent::runner::CompactionBoundaryDecision::Compact
                                } else {
                                    crate::agent::runner::CompactionBoundaryDecision::Continue
                                })
                                .await
                                .unwrap();
                        }
                    }
                    AgentEvent::CompactionBoundary { .. } => {
                        terminal = Some(RetiredNativeOutcome::LiveToolReply)
                    }
                    AgentEvent::Error { .. } => terminal = Some(RetiredNativeOutcome::Incomplete),
                    AgentEvent::Done { .. } | AgentEvent::AwaitingUser { .. } => {
                        panic!("unexpected actor outcome")
                    }
                    _ => {}
                }
            }
            scope.wait_idle().await;
        })
        .await
        .expect("exact actor retirement and owned-child settlement");
        assert_eq!(scope.active_children(), 0);
        assert!(runner.abort_handle.is_finished());
        assert_eq!(calls.load(Ordering::Acquire), 2);
        assert_eq!(session.total_input_tokens, 10);
        assert_eq!(session.total_output_tokens, 2);
        assert!((session.total_cost - 0.000028).abs() < 1e-12);
        let outcome = terminal.expect("actual pressure/error lifecycle event");
        assert_eq!(
            outcome,
            if park {
                RetiredNativeOutcome::LiveToolReply
            } else {
                RetiredNativeOutcome::Incomplete
            }
        );
        let settled = binding.settle_into_with_outcome(&mut session, outcome);
        if park {
            assert_eq!(settled, Ok(true));
            let restored = crate::session::storage::decode_session_json(
                &serde_json::to_string(&session).unwrap(),
            )
            .unwrap();
            let history = restored.native_history_snapshot().unwrap();
            assert_eq!(history.saved_boundary(), Ok(ReplayBoundary::LiveToolReply));
            assert_eq!(
                history.require_completed_boundary(),
                Err(RestartReason::IncompleteTurn)
            );
            assert_eq!(
                history.frames().last().unwrap().native_tool_results.len(),
                2
            );
            assert!(
                restored
                    .native_restore_history(
                        crate::session::native_replay::HistoryProtocol::AnthropicMessages,
                        ReplayBoundary::LiveToolReply
                    )
                    .is_ok()
            );
            assert!(
                restored
                    .native_restore_history(
                        crate::session::native_replay::HistoryProtocol::AnthropicMessages,
                        ReplayBoundary::CompletedTurn
                    )
                    .is_err()
            );
        } else {
            assert_eq!(settled, Err(RestartReason::IncompleteTurn));
            assert_eq!(
                session.native_frontier.refusal(),
                Some(RestartReason::IncompleteTurn)
            );
            assert!(session.native_history_snapshot().is_none());
            let restored = crate::session::storage::decode_session_json(
                &serde_json::to_string(&session).unwrap(),
            )
            .unwrap();
            assert_eq!(
                restored.native_frontier.refusal(),
                Some(RestartReason::IncompleteTurn)
            );
        }
        assert!(session.permission_allowlist.is_empty());
        assert_eq!(wire.finish().len(), 1);
    }
}

async fn native_question_retired_outcome_control() {
    use crate::event::AgentEvent;
    use crate::session::native_replay::{ReplayBoundary, RestartReason, RetiredNativeOutcome};
    let response = start()
        + &thinking(0, "", "parked question signature")
        + &event(
            "content_block_start",
            json!({"type":"content_block_start","index":1,
            "content_block":{"type":"tool_use","id":"toolu_question","name":"ask_user_question","input":{}}}),
        )
        + &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":1,
            "delta":{"type":"input_json_delta","partial_json":json!({"question":"Which target?"}).to_string()}}),
        )
        + &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":1}),
        )
        + &event(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":2}}),
        )
        + &event("message_stop", json!({"type":"message_stop"}));
    let wire = Wire::new(vec![(true, response)]);
    let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
        "claude-opus-5-5",
        crate::config::PromptCacheTtl::default(),
    ) else {
        unreachable!()
    };
    let mut session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    session.set_input_output_prices(Some((2.0, 4.0)));
    let question = crate::question::QuestionTurn::default();
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Actual parked clarification lifecycle")
            .max_tokens(1000)
            .default_max_turns(2)
            .tool(crate::agent::tools::ask_user::AskUserTool::new(
                None,
                crate::config::HeadlessQuestions::Fail,
            ))
            .build(),
    ))
    .with_question_turn(Some(question.clone()))
    .bind_native_turn(
        &session,
        Vec::<Message>::new().into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Lifecycle,
    )
    .unwrap();
    session.add_message(crate::session::MessageRole::User, "literal question turn");
    let scope = crate::agent::runner::AgentWorkScope::new();
    let paused = crate::startup::heap_future(|| {
        agent.spawn_runner_paused_in_scope(
            "literal question turn".into(),
            binding.history(),
            RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            #[cfg(feature = "hooks")]
            None,
            scope.clone(),
        )
    })
    .await;
    let mut runner = paused.start_interactive();
    let mut parked = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                AgentEvent::ToolCall { id, name, args } => {
                    session.add_tool_call_with_id(&id, &name, &args)
                }
                AgentEvent::ToolResult {
                    id, name, output, ..
                } => {
                    session.add_tool_result_with_id_and_artifact(&id, &name, &output);
                }
                AgentEvent::UsageDelta {
                    usage,
                    awaits_compaction_decision,
                    ..
                } => {
                    session.charge_usage_delta(usage, true);
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
                AgentEvent::AwaitingUser {
                    question: required, ..
                } => {
                    assert_eq!(required.id, question.id());
                    session.pending_question = Some(*required);
                    parked = true;
                }
                AgentEvent::Done { .. }
                | AgentEvent::Error { .. }
                | AgentEvent::CompactionBoundary { .. } => {
                    panic!("unexpected question actor terminal")
                }
                _ => {}
            }
        }
        scope.wait_idle().await;
    })
    .await
    .expect("exact parked-question actor retirement");
    assert!(parked);
    assert!(runner.abort_handle.is_finished());
    assert_eq!(scope.active_children(), 0);
    assert_eq!(
        binding.settle_into_with_outcome(&mut session, RetiredNativeOutcome::LiveToolReply),
        Ok(true)
    );
    assert_eq!(session.total_input_tokens, 10);
    assert_eq!(session.total_output_tokens, 2);
    assert!((session.total_cost - 0.000028).abs() < 1e-12);
    assert!(session.permission_allowlist.is_empty());
    let restored =
        crate::session::storage::decode_session_json(&serde_json::to_string(&session).unwrap())
            .unwrap();
    let native = restored.native_history_snapshot().unwrap();
    assert_eq!(native.saved_boundary(), Ok(ReplayBoundary::LiveToolReply));
    assert_eq!(
        native.require_completed_boundary(),
        Err(RestartReason::IncompleteTurn)
    );
    assert_eq!(native.frames().last().unwrap().native_tool_results.len(), 1);
    assert_eq!(
        restored.pending_question.as_ref().unwrap().id,
        question.id()
    );
    assert_eq!(wire.finish().len(), 1);
}

#[tokio::test]
async fn effort46_native_terminal_metadata_requires_current_bound_end_turn() {
    use crate::session::native_replay::ReplayBoundary;
    use rig::completion::GetTokenUsage;
    let forged: rig::providers::anthropic::streaming::StreamingCompletionResponse =
        serde_json::from_value(
            json!({"usage":{"input_tokens":10,"output_tokens":2},"native_end_turn":true}),
        )
        .unwrap();
    assert!(
        !forged.native_anthropic_end_turn(),
        "deserialized data cannot mint a native terminal witness"
    );
    assert_eq!(forged.token_usage().input_tokens, 10);
    assert!(!().native_anthropic_end_turn());
    for (kind, reason, complete) in [
        ("signed", Some("end_turn"), true),
        ("redacted", Some("end_turn"), true),
        ("signed", Some("max_tokens"), false),
        ("redacted", Some("pause_turn"), false),
        ("signed", Some("stop_sequence"), false),
        ("redacted", Some("refusal"), false),
        ("signed", Some("unknown-reason"), false),
        ("signed", None, false),
        ("unsigned", Some("end_turn"), false),
        ("text", Some("end_turn"), false),
        ("interrupted", Some("end_turn"), false),
        ("unbound", Some("end_turn"), false),
    ] {
        let blocks = match kind {
            "signed" | "unbound" => thinking(0, "", "end-kind-signature"),
            "interrupted" => {
                event(
                    "content_block_start",
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                ) + &event(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"interrupted-signature"}}),
                )
            }
            "redacted" => redacted(0),
            "unsigned" => thinking(0, "unsigned thought", ""),
            "text" => text(0, ""),
            _ => unreachable!(),
        };
        let response = start()
            + &blocks
            + &event(
                "message_delta",
                json!({"type":"message_delta",
            "delta":{"stop_reason":reason,"stop_sequence":null},"usage":{"output_tokens":2}}),
            )
            + &event("message_stop", json!({"type":"message_stop"}));
        let wire = Wire::new(vec![(true, response)]);
        let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
            "claude-opus-5-5",
            crate::config::PromptCacheTtl::default(),
        ) else {
            unreachable!()
        };
        let session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
        let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(
            rig::agent::AgentBuilder::new(model)
                .preamble("Native exact stop-kind control")
                .max_tokens(1000)
                .default_max_turns(1)
                .build(),
        ));
        let (agent, history) = if kind == "unbound" {
            (agent, Vec::<Message>::new().into())
        } else {
            let (agent, binding) = agent
                .bind_native_turn(
                    &session,
                    Vec::<Message>::new().into(),
                    ReplayBoundary::CompletedTurn,
                    crate::provider::RecordedToolIds::Canonical,
                )
                .unwrap();
            (agent, binding.history())
        };
        let turn = agent
            .run_print_with_output(
                "literal stop-kind control",
                crate::print::HeadlessOutputPolicy::legacy(false, true),
                &RetryConfig {
                    max_attempts: 1,
                    ..Default::default()
                },
                history,
                #[cfg(feature = "hooks")]
                None,
            )
            .await;
        assert_eq!(
            turn.failure.is_none(),
            complete,
            "kind={kind} reason={reason:?}"
        );
        assert!(turn.response.is_empty());
        if reason.is_some() {
            assert_eq!(turn.usage.input_tokens, 10);
            assert_eq!(turn.usage.output_tokens, 2);
        }
        assert_eq!(
            wire.finish().len(),
            1,
            "no uncounted automatic model request"
        );
    }
}

#[tokio::test]
async fn effort46_native_canonical_stale_prefix_keeps_observed_usage_once() {
    use crate::session::native_replay::{ReplayBoundary, RestartReason};
    let wire = Wire::new(vec![(
        true,
        start()
            + &thinking(0, "", "stale prefix canonical signature")
            + &text(1, "observed completed answer")
            + &finish(),
    )]);
    let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
        "claude-opus-5-5",
        crate::config::PromptCacheTtl::default(),
    ) else {
        unreachable!()
    };
    let mut session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    session.add_message(crate::session::MessageRole::User, "actual prior user bytes");
    session.set_input_output_prices(Some((2.0, 4.0)));
    let prices =
        crate::print::HeadlessPricing::from_session(&session, &crate::config::Config::default());
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Canonical stale-prefix accounting")
            .max_tokens(1000)
            .default_max_turns(1)
            .build(),
    ))
    .bind_native_turn(
        &session,
        vec![Message::user("actual prior user bytes")].into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Canonical,
    )
    .unwrap();
    let turn = agent
        .run_print_with_output(
            "literal observed work",
            crate::print::HeadlessOutputPolicy::legacy(false, true),
            &RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            binding.history(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(turn.failure.is_none());
    assert!(binding.accepted_progress());
    let usage: crate::event::UsageDelta = turn.usage.into();
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 2);
    let mut foreign = session.clone();
    foreign.id = "foreign-session".into();
    let foreign_before = serde_json::to_value(&foreign).unwrap();
    assert_eq!(
        binding.commit_canonical_into(
            &mut foreign,
            "literal observed work",
            &turn.response,
            &turn.interactions,
            usage,
            prices,
        ),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(
        serde_json::to_value(&foreign).unwrap(),
        foreign_before,
        "foreign Session cannot receive this usage or denial"
    );
    let mut newer = session.clone();
    newer.native_frontier = session.native_frontier.fork();
    newer.native_frontier.retire().unwrap();
    let newer_before = serde_json::to_value(&newer).unwrap();
    assert_eq!(
        binding.commit_canonical_into(
            &mut newer,
            "literal observed work",
            &turn.response,
            &turn.interactions,
            usage,
            prices,
        ),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(
        serde_json::to_value(&newer).unwrap(),
        newer_before,
        "new cutoff cannot receive old captured usage or denial"
    );
    session.messages[0].content = "edited prior bytes at same row count".into();
    session.set_input_output_prices(Some((200.0, 400.0)));
    let edited = serde_json::to_value(&session.messages).unwrap();
    assert_eq!(
        binding.commit_canonical_into(
            &mut session,
            "literal observed work",
            &turn.response,
            &turn.interactions,
            usage,
            prices,
        ),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(
        serde_json::to_value(&session.messages).unwrap(),
        edited,
        "refused attachment cannot append to the edited feed"
    );
    assert_eq!(
        session.total_input_tokens, 10,
        "observed canonical usage survives refused native attachment"
    );
    assert_eq!(session.total_output_tokens, 2);
    assert!(
        (session.total_cost - 0.000028).abs() < 1e-12,
        "captured prices, not the changed live prices"
    );
    assert_eq!(
        session.native_frontier.refusal(),
        Some(RestartReason::ChangedPrefix)
    );
    assert!(session.native_history_snapshot().is_none());
    let once = serde_json::to_value(&session).unwrap();
    assert_eq!(
        binding.commit_canonical_into(
            &mut session,
            "literal observed work",
            &turn.response,
            &turn.interactions,
            usage,
            prices,
        ),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(
        serde_json::to_value(&session).unwrap(),
        once,
        "duplicate retirement cannot charge twice"
    );
    assert_eq!(wire.finish().len(), 1);
}

#[tokio::test]
async fn effort46_native_canonical_commit_once_does_not_refuse_attached_chain() {
    use crate::session::native_replay::{ReplayBoundary, RestartReason, RetiredNativeOutcome};
    let wire = Wire::new(vec![(
        true,
        start()
            + &thinking(0, "", "completed canonical signature")
            + &redacted(1)
            + &text(2, "settled")
            + &finish(),
    )]);
    let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
        "claude-opus-5-5",
        crate::config::PromptCacheTtl::default(),
    ) else {
        unreachable!()
    };
    let mut session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    session.set_input_output_prices(Some((2.0, 4.0)));
    let prices =
        crate::print::HeadlessPricing::from_session(&session, &crate::config::Config::default());
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Native canonical once control")
            .max_tokens(1000)
            .default_max_turns(1)
            .build(),
    ))
    .bind_native_turn(
        &session,
        Vec::<Message>::new().into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Canonical,
    )
    .unwrap();
    let turn = agent
        .run_print_with_output(
            "literal canonical once",
            crate::print::HeadlessOutputPolicy::legacy(false, true),
            &RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            binding.history(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(turn.failure.is_none());
    assert!(binding.accepted_progress());
    let usage: crate::event::UsageDelta = turn.usage.into();
    assert_eq!(
        binding.commit_canonical_into_with_outcome(
            &mut session,
            "literal canonical once",
            &turn.response,
            &turn.interactions,
            usage,
            prices,
            RetiredNativeOutcome::Completed
        ),
        Ok(true)
    );
    assert_eq!(session.native_frontier.refusal(), None);
    assert_eq!(session.native_history_snapshot().unwrap().frames().len(), 1);
    assert_eq!(session.total_input_tokens, 10);
    assert_eq!(session.total_output_tokens, 2);
    assert!((session.total_cost - 0.000028).abs() < 1e-12);
    let once = serde_json::to_value(&session).unwrap();
    assert_eq!(
        binding.commit_canonical_into_with_outcome(
            &mut session,
            "literal canonical once",
            &turn.response,
            &turn.interactions,
            usage,
            prices,
            RetiredNativeOutcome::Completed
        ),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(
        serde_json::to_value(&session).unwrap(),
        once,
        "an already committed native binding must not fence its own successfully attached chain"
    );
    assert_eq!(wire.finish().len(), 1);
}

#[tokio::test]
async fn effort46_untracked_opaque_initial_history_refuses_before_native_http() {
    use crate::session::native_replay::{ReplayBoundary, RestartReason};
    let wire = Wire::new(vec![(
        true,
        start() + &text(0, "unexpected request") + &finish(),
    )]);
    let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
        "claude-opus-5-5",
        crate::config::PromptCacheTtl::default(),
    ) else {
        unreachable!()
    };
    let session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    let generic = vec![
        Message::user("prior literal user"),
        Message::Assistant {
            id: None,
            content: rig::OneOrMany::one(rig::completion::AssistantContent::Reasoning(
                rig::message::Reasoning::new_with_signature(
                    "",
                    Some("unbound legacy signature".into()),
                ),
            )),
        },
    ];
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Untracked initial prefix")
            .max_tokens(1000)
            .default_max_turns(1)
            .build(),
    ))
    .bind_native_turn(
        &session,
        generic.into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Canonical,
    )
    .unwrap();
    let before = serde_json::to_value(&session).unwrap();
    let turn = agent
        .run_print_with_output(
            "literal current user",
            crate::print::HeadlessOutputPolicy::legacy(false, true),
            &RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            binding.history(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert_eq!(binding.refusal(), Some(RestartReason::UnavailableSource));
    assert!(turn.failure.is_some());
    assert!(!binding.accepted_progress());
    assert_eq!(turn.usage.total_tokens, 0);
    assert_eq!(serde_json::to_value(&session).unwrap(), before);
    assert!(
        wire.finish().is_empty(),
        "untracked signed history reached native HTTP"
    );
}

#[tokio::test]
async fn effort46_native_prepared_capture_refuses_raw_binding_overrides_before_connection() {
    use crate::session::native_replay::RestartReason;
    for key in ["system", "messages", "tools", "model"] {
        let wire = Wire::new(Vec::new());
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let capture =
            crate::agent::native_capture::NativeCapture::for_replay(model.clone(), true).unwrap();
        let mut raw = serde_json::Map::new();
        raw.insert(key.to_string(), json!("untrusted raw replacement"));
        let inner = rig::agent::AgentBuilder::new(model)
            .preamble("Stable native probe system")
            .max_tokens(1000)
            .default_max_turns(1)
            .additional_params(Value::Object(raw))
            .build();
        let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(inner))
            .with_native_capture(capture.clone());
        let turn = agent
            .run_print_with_output(
                "literal initial user",
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
            capture.replay_failure(),
            Some(RestartReason::UnsupportedOverride),
            "raw {key} override must be refused as unsupported"
        );
        assert!(
            turn.failure.is_some(),
            "raw {key} override must fail the turn"
        );
        let (frames, unavailable) = capture.snapshot();
        assert!(frames.is_empty(), "raw {key} override captured frames");
        assert!(unavailable.is_none());
        assert_eq!(
            capture.persisted_history().err(),
            Some(RestartReason::UnsupportedOverride),
            "raw {key} override must not yield a persisted base"
        );
        assert_eq!(turn.usage.input_tokens, 0);
        assert_eq!(turn.usage.output_tokens, 0);
        assert_eq!(turn.usage.total_tokens, 0);
        assert!(
            wire.finish().is_empty(),
            "raw binding refusal attempted TCP/HTTP for {key}"
        );
    }
}

#[tokio::test]
async fn effort46_completed_native_capture_limit_preserves_accepted_denial_and_usage() {
    use crate::session::native_replay::{ReplayBoundary, RestartReason};
    let opaque = "x".repeat(2 * 1024 * 1024 + 1);
    let response = start()
        + &event(
            "content_block_start",
            json!({
                "type":"content_block_start", "index":0,
                "content_block":{"type":"redacted_thinking","data":opaque}
            }),
        )
        + &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        )
        + &text(1, "settled")
        + &finish();
    let wire = Wire::new(vec![(true, response)]);
    let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
        "claude-opus-5-5",
        crate::config::PromptCacheTtl::default(),
    ) else {
        unreachable!()
    };
    let mut session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    session.set_input_output_prices(Some((2.0, 4.0)));
    let prices =
        crate::print::HeadlessPricing::from_session(&session, &crate::config::Config::default());
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Bounded capture control")
            .max_tokens(1000)
            .default_max_turns(1)
            .build(),
    ))
    .bind_native_turn(
        &session,
        Vec::<Message>::new().into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Canonical,
    )
    .unwrap();
    let turn = agent
        .run_print_with_output(
            "literal limited capture",
            crate::print::HeadlessOutputPolicy::legacy(false, true),
            &RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            binding.history(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert_eq!(turn.usage.input_tokens, 10);
    assert_eq!(turn.usage.output_tokens, 2);
    assert!(
        binding.accepted_progress(),
        "completed content exceeding capture limits is accepted progress"
    );
    assert_eq!(
        binding.commit_canonical_into(
            &mut session,
            "literal limited capture",
            &turn.response,
            &turn.interactions,
            turn.usage.into(),
            prices
        ),
        Err(RestartReason::UnavailableSource)
    );
    assert_eq!(session.total_input_tokens, 10);
    assert_eq!(session.total_output_tokens, 2);
    assert!((session.total_cost - 0.000028).abs() < 1e-12);
    assert_eq!(
        session.native_frontier.refusal(),
        Some(RestartReason::UnavailableSource)
    );
    assert!(session.native_history_snapshot().is_none());
    let restored =
        crate::session::storage::decode_session_json(&serde_json::to_string(&session).unwrap())
            .unwrap();
    assert_eq!(
        restored.native_frontier.refusal(),
        Some(RestartReason::UnavailableSource)
    );
    assert_eq!(wire.finish().len(), 1);
}

#[tokio::test]
async fn effort46_canonical_commit_generic_once_stale_and_captured_prices_without_receipts() {
    use crate::session::native_replay::{ReplayBoundary, RestartReason};
    let wire = Wire::new(Vec::new());
    let AnyModel::OpenRouter(model, _) = wire
        .client_for("openrouter")
        .completion_model_with_cache_ttl(
            "anthropic/claude-opus-5-5",
            crate::config::PromptCacheTtl::default(),
        )
    else {
        unreachable!()
    };
    let mut session =
        crate::session::Session::new("anthropic-label", "anthropic/claude-opus-5-5", 32_000, "");
    session.set_input_output_prices(Some((2.0, 4.0)));
    let cfg = crate::config::Config::default();
    let pricing = crate::print::HeadlessPricing::from_session(&session, &cfg);
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::OpenRouter(
        rig::agent::AgentBuilder::new(model).build(),
    ))
    .bind_native_turn(
        &session,
        Vec::<Message>::new().into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Canonical,
    )
    .unwrap();
    assert_eq!(
        agent.history_protocol(),
        crate::session::native_replay::HistoryProtocol::OpenRouterChatCompletions
    );
    assert!(!binding.accepted_progress());
    let initial = serde_json::to_value(&session).unwrap();
    let mut stale = session.clone();
    stale.id = "different-logical-session".into();
    let before = serde_json::to_value(&stale).unwrap();
    let usage = crate::event::UsageDelta {
        input_tokens: 10,
        output_tokens: 2,
        total_tokens: 12,
        ..Default::default()
    };
    assert_eq!(
        binding.commit_canonical_into(&mut stale, "literal generic", "answer", &[], usage, pricing),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(serde_json::to_value(&stale).unwrap(), before);
    assert_eq!(serde_json::to_value(&session).unwrap(), initial);
    // Live display rates can change after admission; the owned turn uses its
    // captured projection prices, while no model report/receipt is fabricated.
    session.set_input_output_prices(Some((99.0, 100.0)));
    assert_eq!(
        binding.commit_canonical_into(
            &mut session,
            "literal generic",
            "answer",
            &[],
            usage,
            pricing
        ),
        Ok(false)
    );
    assert_eq!(session.total_input_tokens, 10);
    assert_eq!(session.total_output_tokens, 2);
    assert!((session.total_cost - 0.000028).abs() < 1e-12);
    assert!(session.native_history_snapshot().is_none());
    let once = serde_json::to_value(&session).unwrap();
    assert_eq!(
        binding.commit_canonical_into(
            &mut session,
            "literal generic",
            "answer",
            &[],
            usage,
            pricing
        ),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(serde_json::to_value(&session).unwrap(), once);
    assert_eq!(session.messages.len(), 2);
    assert!(session.spend_account.is_uninitialised());
    assert!(session.permission_allowlist.is_empty());
    use crate::agent::input::PromptInput;
    use crate::session::user::{ImageMime, PersistedUserMessage, UserBlock};
    let user = PersistedUserMessage::new(vec![
        UserBlock::Text {
            text: "before image".into(),
        },
        UserBlock::Image {
            data: "aW5lcnQ=".into(),
            mime_type: ImageMime::Png,
        },
        UserBlock::Text {
            text: "after image".into(),
        },
    ])
    .unwrap();
    let prompt = PromptInput::user(user.clone());
    let mut typed = crate::session::Session::new("generic", "model", 32_000, "");
    let (_, typed_binding) = agent
        .clone()
        .bind_native_turn(
            &typed,
            Vec::<Message>::new().into(),
            ReplayBoundary::CompletedTurn,
            crate::provider::RecordedToolIds::Canonical,
        )
        .unwrap();
    assert_eq!(
        typed_binding.commit_canonical_input_into(
            &mut typed,
            &prompt,
            "typed answer",
            &[Message::user("Please continue.")],
            usage,
            pricing,
        ),
        Ok(false)
    );
    assert_eq!(typed.messages[0].user, Some(user));
    assert_eq!(typed.messages[0].content, prompt.literal_text());
    assert_eq!(typed.messages[1].content, "Please continue.");
    assert_eq!(typed.total_input_tokens, 10);
    assert_eq!(typed.total_output_tokens, 2);
    let imported =
        crate::session::storage::decode_session_json(&serde_json::to_string(&typed).unwrap())
            .unwrap();
    assert_eq!(imported.messages[0].user, typed.messages[0].user);
    let snapshot = serde_json::to_value(&typed).unwrap();
    assert_eq!(
        typed_binding.commit_canonical_input_into(
            &mut typed,
            &prompt,
            "typed answer",
            &[],
            usage,
            pricing,
        ),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(serde_json::to_value(&typed).unwrap(), snapshot);

    let Message::User { content } = prompt.to_message() else {
        unreachable!()
    };
    let mut image = content
        .into_iter()
        .find_map(|block| match block {
            rig::message::UserContent::Image(image) => Some(image),
            _ => None,
        })
        .unwrap();
    image.data =
        rig::message::DocumentSourceKind::Url("https://unresolved-image.example.test".into());
    let incompatible = Message::User {
        content: rig::OneOrMany::one(rig::message::UserContent::Image(image)),
    };
    let mut failed = crate::session::Session::new("generic", "model", 32_000, "");
    let (_, failed_binding) = agent
        .clone()
        .bind_native_turn(
            &failed,
            Vec::<Message>::new().into(),
            ReplayBoundary::CompletedTurn,
            crate::provider::RecordedToolIds::Canonical,
        )
        .unwrap();
    assert_eq!(
        failed_binding.commit_canonical_input_into(
            &mut failed,
            &prompt,
            "observed answer",
            &[incompatible],
            usage,
            pricing,
        ),
        Err(RestartReason::UnavailableSource)
    );
    assert_eq!(failed.total_input_tokens, 10);
    assert_eq!(failed.total_output_tokens, 2);
    assert!((failed.total_cost - 0.000028).abs() < 1e-12);
    let snapshot = serde_json::to_value(&failed).unwrap();
    assert_eq!(
        failed_binding.commit_canonical_input_into(
            &mut failed,
            &prompt,
            "observed answer",
            &[],
            usage,
            pricing,
        ),
        Err(RestartReason::ChangedPrefix)
    );
    assert_eq!(serde_json::to_value(&failed).unwrap(), snapshot);
    assert!(failed.spend_account.is_uninitialised());
    assert!(
        wire.finish().is_empty(),
        "canonical projection attempted HTTP"
    );
}

#[tokio::test]
async fn effort46_native_refused_checkpoint_preserves_observed_canonical_usage() {
    native_refused_checkpoint_controls(false).await;
}

#[tokio::test]
async fn effort46_native_refused_checkpoint_preserves_observed_interactive_usage() {
    native_refused_checkpoint_controls(true).await;
}

async fn native_refused_checkpoint_controls(interactive: bool) {
    use crate::session::native_replay::{ReplayBoundary, RestartReason};
    let roots = crate::tests::OwnedTestDirectory::new("native-refused-checkpoint");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "ZS_CONFIG_DIR",
            Some(roots.path().join("config").into_os_string()),
        ),
        (
            "ZS_DATA_DIR",
            Some(roots.path().join("data").into_os_string()),
        ),
        (
            "ZS_STATE_DIR",
            Some(roots.path().join("state").into_os_string()),
        ),
        (
            "ZS_CACHE_DIR",
            Some(roots.path().join("cache").into_os_string()),
        ),
    ]);
    struct Release(String);
    impl Drop for Release {
        fn drop(&mut self) {
            crate::session::lock::release_session(&self.0);
        }
    }
    let wire = Wire::new(vec![(
        true,
        start()
            + &thinking(0, "", "accepted refused-checkpoint signature")
            + &text(1, "observed completed answer")
            + &finish(),
    )]);
    let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
        "claude-opus-5-5",
        crate::config::PromptCacheTtl::default(),
    ) else {
        unreachable!()
    };
    let mut session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    let _release = Release(session.id.to_string());
    session.add_message(crate::session::MessageRole::User, "prior user bytes");
    session.set_input_output_prices(Some((2.0, 4.0)));
    let cfg = crate::config::Config::default();
    session.initialize_spend_account(None).unwrap();
    let prices = crate::print::HeadlessPricing::from_session(&session, &cfg);
    let io_scope = crate::agent::runner::AgentWorkScope::new();
    let mut ticket = crate::session::native_publication::begin_owned(&mut session, &io_scope)
        .await
        .unwrap();
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Exact accepted-refusal publication")
            .max_tokens(1000)
            .default_max_turns(1)
            .build(),
    ))
    .bind_native_turn(
        &session,
        vec![Message::user("prior user bytes")].into(),
        ReplayBoundary::CompletedTurn,
        if interactive {
            crate::provider::RecordedToolIds::Lifecycle
        } else {
            crate::provider::RecordedToolIds::Canonical
        },
    )
    .unwrap();
    binding.pair_publication(&session, &mut ticket).unwrap();
    assert!(binding.pair_publication(&session, &mut ticket).is_err());
    let agent = agent.with_spend_for_session(&mut session, &cfg, crate::spend::RequestKind::Turn);
    if interactive {
        use crate::event::AgentEvent;
        let scope = crate::agent::runner::AgentWorkScope::new();
        session.add_message(crate::session::MessageRole::User, "literal accepted work");
        let paused = crate::startup::heap_future(|| {
            agent.spawn_runner_paused_in_scope(
                "literal accepted work".into(),
                binding.history(),
                RetryConfig {
                    max_attempts: 1,
                    ..Default::default()
                },
                #[cfg(feature = "hooks")]
                None,
                scope.clone(),
            )
        })
        .await;
        let mut runner = paused.start_interactive();
        let mut done = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = runner.event_rx.recv().await {
                match event {
                    AgentEvent::UsageDelta {
                        usage,
                        awaits_compaction_decision,
                        ..
                    } => {
                        session.charge_usage_delta(usage, true);
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
                    AgentEvent::Done { .. } => done = true,
                    AgentEvent::Error { message, .. } => {
                        panic!("paired native actor failed: {message}")
                    }
                    _ => {}
                }
            }
            scope.wait_idle().await;
        })
        .await
        .expect("paired native actor retirement");
        assert!(done && runner.abort_handle.is_finished());
        assert_eq!(scope.active_children(), 0);
        assert!(binding.accepted_progress());
        session.messages[0].content = "edited prior bytes, same row count".into();
        assert_eq!(
            binding.settle_into_with_outcome(
                &mut session,
                crate::session::native_replay::RetiredNativeOutcome::Completed
            ),
            Err(RestartReason::ChangedPrefix)
        );
    } else {
        let turn = agent
            .run_print_with_output(
                "literal accepted work",
                crate::print::HeadlessOutputPolicy::legacy(false, true),
                &RetryConfig {
                    max_attempts: 1,
                    ..Default::default()
                },
                binding.history(),
                #[cfg(feature = "hooks")]
                None,
            )
            .await;
        assert!(turn.failure.is_none());
        assert!(binding.accepted_progress());
        let usage: crate::event::UsageDelta = turn.usage.into();
        assert_eq!((usage.input_tokens, usage.output_tokens), (10, 2));
        session.messages[0].content = "edited prior bytes, same row count".into();
        assert_eq!(
            binding.commit_canonical_into(
                &mut session,
                "literal accepted work",
                &turn.response,
                &turn.interactions,
                usage,
                prices
            ),
            Err(RestartReason::ChangedPrefix)
        );
    }
    assert_eq!(
        session.native_frontier.refusal(),
        Some(RestartReason::ChangedPrefix)
    );
    assert_eq!(
        (session.total_input_tokens, session.total_output_tokens),
        (10, 2)
    );
    assert!((session.total_cost - 0.000028).abs() < 1e-12);
    // Actual accepted binding advanced its own cutoff. Its current mutable
    // publisher must still persist canonical observation data as REFUSED.
    binding
        .publish_refused_owned(&mut session, ticket, &io_scope)
        .await
        .expect("accepted binding refusal must preserve its canonical checkpoint");
    io_scope.wait_idle().await;
    let mut loaded = crate::session::storage::load_session_exact(&session.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded.native_frontier.refusal(),
        Some(RestartReason::ChangedPrefix)
    );
    assert_eq!(
        (loaded.total_input_tokens, loaded.total_output_tokens),
        (10, 2)
    );
    assert!((loaded.total_cost - 0.000028).abs() < 1e-12);
    assert!(loaded.native_history_snapshot().is_none());
    loaded.initialize_spend_account(Some(1.0)).unwrap();
    loaded.initialize_spend_account(Some(1.0)).unwrap();
    assert!(loaded.spend_account.snapshot().tracking_error);
    assert!((loaded.recorded_total_cost() - 0.000028).abs() < 1e-12);
    let ledger = crate::spend::Ledger::new(&crate::paths::process_paths().unwrap().state_dir);
    let summary = ledger.summary_for_session(&loaded.id).unwrap();
    assert_eq!(summary.session.records, 1);
    assert!((summary.session.known_cost - 0.000028).abs() < 1e-12);
    assert_eq!(wire.finish().len(), 1);
}

#[tokio::test]
async fn effort46_restored_inflight_after_opened_unobserved_http_keeps_account_incomplete() {
    use crate::session::native_replay::{ReplayBoundary, RestartReason};
    let roots = crate::tests::OwnedTestDirectory::new("native-restored-http-uncertainty");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "ZS_CONFIG_DIR",
            Some(roots.path().join("config").into_os_string()),
        ),
        (
            "ZS_DATA_DIR",
            Some(roots.path().join("data").into_os_string()),
        ),
        (
            "ZS_STATE_DIR",
            Some(roots.path().join("state").into_os_string()),
        ),
        (
            "ZS_CACHE_DIR",
            Some(roots.path().join("cache").into_os_string()),
        ),
    ]);
    struct Release(String);
    impl Drop for Release {
        fn drop(&mut self) {
            crate::session::lock::release_session(&self.0);
        }
    }
    let wire = Wire::new(vec![(
        true,
        start() + "event: content_block_delta\ndata: {invalid incomplete native response}\n\n",
    )]);
    let AnyModel::Anthropic(model) = wire.client().completion_model_with_cache_ttl(
        "claude-opus-5-5",
        crate::config::PromptCacheTtl::default(),
    ) else {
        unreachable!()
    };
    let mut session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    let _release = Release(session.id.to_string());
    session.set_input_output_prices(Some((2.0, 4.0)));
    let cfg = crate::config::Config {
        max_budget_usd: Some(1.0),
        retry: RetryConfig {
            max_attempts: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    session
        .initialize_spend_account(cfg.max_budget_usd)
        .unwrap();
    let io = crate::agent::runner::AgentWorkScope::new();
    let mut ticket = crate::session::native_publication::begin_owned(&mut session, &io)
        .await
        .unwrap();
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .max_tokens(1000)
            .default_max_turns(1)
            .build(),
    ))
    .bind_native_turn(
        &session,
        Vec::<Message>::new().into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Canonical,
    )
    .unwrap();
    binding.pair_publication(&session, &mut ticket).unwrap();
    let turn = agent
        .with_spend_for_session(&mut session, &cfg, crate::spend::RequestKind::Turn)
        .run_print_with_output(
            "actual unknown paid observation",
            crate::print::HeadlessOutputPolicy::legacy(false, false),
            &cfg.retry,
            binding.history(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(turn.failure.is_some());
    assert!(!binding.accepted_progress());
    assert!(
        session.spend_account.snapshot().tracking_error,
        "actual opened200 without accepted usage is incomplete live"
    );
    drop(ticket);
    crate::session::lock::release_session(&session.id);
    let mut restored = crate::session::storage::load_session_exact(&session.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        restored.native_frontier.refusal(),
        Some(RestartReason::InFlight)
    );
    assert!(
        !restored.spend_account.snapshot().tracking_error,
        "old durable preflight predates HTTP retirement"
    );
    restored
        .initialize_spend_account(cfg.max_budget_usd)
        .unwrap();
    assert!(restored.spend_account.snapshot().tracking_error);
    let run =
        crate::spend::run::Run::for_session(&mut restored, &cfg, crate::spend::RequestKind::Turn);
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(io.clone());
    assert!(io.run(run.prepare()).await.is_err());
    cleanup.settle().await;
    assert_eq!(restored.recorded_total_cost(), 0.0);
    assert_eq!(
        (restored.total_input_tokens, restored.total_output_tokens),
        (0, 0)
    );
    let ledger = crate::spend::Ledger::new(&crate::paths::process_paths().unwrap().state_dir);
    assert_eq!(
        ledger
            .summary_for_session(&restored.id)
            .unwrap()
            .session
            .records,
        0
    );
    assert_eq!(
        wire.finish().len(),
        1,
        "no capped retry or invented receipt"
    );
}

#[tokio::test]
async fn effort46_actual_headless_frontend_publishes_and_reloads_native_context() {
    native_headless_frontend_control(false, false, false, false, false, None).await;
}

#[tokio::test]
async fn effort46_actual_headless_frontend_changed_host_refuses_before_http() {
    native_headless_frontend_control(true, false, false, false, false, None).await;
}

#[tokio::test]
async fn effort46_actual_headless_frontend_failed_final_save_keeps_disk_inflight() {
    native_headless_frontend_control(false, true, false, false, false, None).await;
}

#[tokio::test]
async fn effort46_actual_headless_frontend_unadopted_resume_refuses_before_effects() {
    native_headless_frontend_control(false, false, true, false, false, None).await;
}

async fn native_headless_frontend_control(
    changed_host: bool,
    fail_final_save: bool,
    unadopted_resume: bool,
    held_end_hook: bool,
    fail_second_final: bool,
    no_session_case: Option<bool>,
) {
    #[cfg(feature = "hooks")]
    let _dispatcher_owner = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = crate::tests::OwnedTestDirectory::new("native-headless-frontend");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "ZS_CONFIG_DIR",
            Some(roots.path().join("config").into_os_string()),
        ),
        (
            "ZS_DATA_DIR",
            Some(roots.path().join("data").into_os_string()),
        ),
        (
            "ZS_STATE_DIR",
            Some(roots.path().join("state").into_os_string()),
        ),
        (
            "ZS_CACHE_DIR",
            Some(roots.path().join("cache").into_os_string()),
        ),
    ]);
    // Restore the exact previous selection before environment/root cleanup.
    // The existing dispatcher owner serializes all global-hook fixture users.
    #[cfg(feature = "hooks")]
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    struct Release(Option<String>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(id) = &self.0 {
                crate::session::lock::release_session(id);
            }
        }
    }
    #[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
    let _hook_gate = if held_end_hook {
        let release = roots.path().join("end.release");
        let gate = crate::tests::process_gate::ProcessGate::new(&release).unwrap();
        crate::fs::private_atomic_write_sync(&roots.path().join("config/settings.json"),
            serde_json::to_vec(&json!({"hooks":{"SessionEnd":[{"hooks":[
                {"type":"command","command":"/bin/sh","args":["-c", r#"printf '%s\n' "$$" > "$1"; IFS= read -r release < "$2""#, "held-end", roots.path().join("end.pid"), release], "trust":"trusted","async":true,"timeout":60},
                {"type":"command","command":"/bin/sh","args":["-c", r#"while [ ! -f "$1" ]; do sleep 0.01; done"#, "end-observer", roots.path().join("end.observed")], "trust":"trusted","timeout":15}
            ]}]}})).unwrap().as_slice()).unwrap();
        let settings: serde_json::Value = serde_json::from_slice(
            &std::fs::read(roots.path().join("config/settings.json")).unwrap(),
        )
        .unwrap();
        let config =
            crate::extras::hooks::settings::parse_hooks_config(&settings["hooks"]).unwrap();
        crate::extras::hooks::init_dispatcher(
            crate::extras::hooks::dispatcher::HookDispatcher::from_config(&config).unwrap(),
        );
        crate::extras::hooks::set_active_workspace(roots.path());
        Some(gate)
    } else if no_session_case.is_some() {
        let settings = json!({"SessionStart":[{"hooks":[{
            "type":"command", "command":"/bin/sh",
            "args":["-c", r#"printf 'actual SessionStart\n' >> "$1""#, "start-proof", roots.path().join("start.effects")],
            "trust":"trusted", "timeout":5
        }]}], "UserPromptSubmit":[{"hooks":[{
            "type":"command", "command":"/bin/sh",
            "args":["-c", r#"printf 'actual UserPromptSubmit\n' >> "$1""#, "prompt-proof", roots.path().join("prompt.effects")],
            "trust":"trusted", "timeout":5
        }]}]});
        let config = crate::extras::hooks::settings::parse_hooks_config(&settings).unwrap();
        crate::extras::hooks::init_dispatcher(
            crate::extras::hooks::dispatcher::HookDispatcher::from_config(&config).unwrap(),
        );
        crate::extras::hooks::set_active_workspace(roots.path());
        None
    } else {
        None
    };
    #[cfg(not(all(feature = "hooks", any(target_os = "macos", target_os = "linux"))))]
    assert!(!held_end_hook && no_session_case.is_none());
    #[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
    struct ResetHook;
    #[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
    impl Drop for ResetHook {
        fn drop(&mut self) {
            crate::extras::hooks::reset_dispatcher();
        }
    }
    #[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
    let _hook_reset = if held_end_hook || no_session_case.is_some() {
        Some(ResetHook)
    } else {
        None
    };
    let mut release = Release(None);
    let mut replies = vec![
        (
            true,
            start()
                + &thinking(0, "", "frontend preserved signature")
                + &text(1, "first completed answer")
                + &finish(),
        ),
        (
            true,
            start()
                + &thinking(0, "", "second frontend signature")
                + &text(1, "second completed answer")
                + &finish(),
        ),
    ];
    if held_end_hook || no_session_case == Some(true) {
        replies.truncate(1);
    }
    let wire = Wire::new(replies);
    let cfg: crate::config::Config = serde_json::from_value(json!({
        "session_title_model":"off",
        "custom_providers":{"effort-probe":{"provider_type":"anthropic","base_url":wire.base}},
        "quick_models":{"native-test":{"provider":"effort-probe","model":"claude-opus-5-5","input_token_cost":2.0,"output_token_cost":4.0}}
    })).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
    let paths = crate::paths::process_paths().unwrap();
    let mut id = None;
    for (round, prompt) in ["first literal user input", "second literal user input"]
        .into_iter()
        .enumerate()
    {
        let cli = crate::cli::Cli {
            api_key: Some("unused-native-key".into()),
            quick_model: Some("native-test".into()),
            session: id.clone(),
            no_session: no_session_case.is_some_and(|fresh| fresh || round == 1),
            print: !(unadopted_resume && round == 1),
            quiet: true,
            message: vec![prompt.into()],
            name: Some("native frontend owned test".into()),
            no_context_files: !(changed_host && round == 1),
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let startup = crate::startup::heap_future(|| async {
            let startup = crate::startup::Startup::init(
                cli,
                cfg.clone(),
                paths.clone(),
                workspace.clone(),
                false,
                false,
                false,
            )
            .await?;
            Ok::<_, anyhow::Error>(Box::new(startup))
        })
        .await;
        if (unadopted_resume || no_session_case == Some(false)) && round == 1 {
            let error = startup
                .err()
                .expect("unadopted native resume must stop during init");
            if no_session_case.is_some() {
                assert!(error.to_string().contains("no-session"), "{error:#}");
                assert_eq!(
                    std::fs::read(roots.path().join("start.effects")).unwrap(),
                    b"actual SessionStart\n"
                );
                assert_eq!(
                    std::fs::read(roots.path().join("prompt.effects")).unwrap(),
                    b"actual UserPromptSubmit\n"
                );
            } else {
                assert!(error.to_string().contains("headless print frontend"));
            }
            let saved = crate::session::storage::load_session_exact(id.as_ref().unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(saved.native_frontier.refusal(), None);
            assert_eq!(
                (saved.total_input_tokens, saved.total_output_tokens),
                (10, 2)
            );
            assert_eq!(wire.finish().len(), 1);
            return;
        }
        let mut startup = startup.unwrap();
        assert_eq!(
            (
                startup.session.input_token_cost,
                startup.session.output_token_cost
            ),
            (2.0, 4.0)
        );
        let sid = startup.session.id.to_string();
        release.0 = Some(sid.clone());
        startup.init_features().await.unwrap();
        startup.resolve_prompts().await.unwrap();
        if fail_final_save || (fail_second_final && round == 1) {
            startup.fail_native_final_before_replace();
        }
        let dispatch = crate::startup::heap_future(|| startup.dispatch());
        #[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
        let result = if held_end_hook {
            use crate::tests::process_state::{ProcessIdentity, ProcessState};
            let observer = async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(roots.path().join("end.pid"))
                        && text.ends_with('\n')
                        && let Ok(pid) = text.trim().parse::<u32>()
                    {
                        let identity = ProcessIdentity::capture(pid).unwrap();
                        assert!(matches!(
                            identity.state().unwrap(),
                            ProcessState::Live { .. }
                        ));
                        let saved = crate::session::storage::load_session_exact(&sid)
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            saved.native_frontier.refusal(),
                            Some(crate::session::native_replay::RestartReason::InFlight)
                        );
                        // The sole model request is complete; stop the shared
                        // fixture's accept owner before the intentional red hang.
                        wire.stop.store(true, std::sync::atomic::Ordering::Release);
                        std::fs::write(
                            roots.path().join("end.observed"),
                            b"observed exact live handler",
                        )
                        .unwrap();
                        return identity;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            let (result, identity) = tokio::join!(
                tokio::time::timeout(Duration::from_secs(5), dispatch),
                tokio::time::timeout(Duration::from_secs(5), observer),
            );
            let identity = identity.expect("actual async SessionEnd did not start");
            tokio::time::timeout(Duration::from_secs(5), async {
                while !matches!(
                    identity.state().unwrap(),
                    ProcessState::Gone | ProcessState::Replaced
                ) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("exact SessionEnd handler survived cleanup");
            assert_eq!(
                identity.state().unwrap(),
                ProcessState::Gone,
                "hook must be reaped before healthy publication"
            );
            result.expect("async SessionEnd kept the actual native frontend pending")
        } else {
            dispatch.await
        };
        #[cfg(not(all(feature = "hooks", any(target_os = "macos", target_os = "linux"))))]
        let result = dispatch.await;
        if fail_final_save || ((fail_second_final || changed_host) && round == 1) {
            assert!(
                result.is_err(),
                "unfinished or incompatible context must refuse"
            );
            let saved = crate::session::storage::load_session_exact(&sid)
                .unwrap()
                .unwrap();
            assert_eq!(
                saved.native_frontier.refusal(),
                Some(crate::session::native_replay::RestartReason::InFlight)
            );
            let accepted_rounds = if changed_host { 1 } else { round + 1 };
            assert_eq!(
                (saved.total_input_tokens, saved.total_output_tokens),
                (accepted_rounds as u64 * 10, accepted_rounds as u64 * 2)
            );
            let summary = crate::spend::Ledger::new(&paths.state_dir)
                .summary_for_session(&sid)
                .unwrap();
            assert_eq!(summary.session.records, accepted_rounds);
            assert!((summary.session.known_cost - accepted_rounds as f64 * 0.000028).abs() < 1e-12);
            assert!(saved.native_publication.is_none());
            if fail_second_final {
                crate::session::lock::release_session(&sid);
                #[cfg(unix)]
                crate::session::native_publication::assert_refused_reload_in_separate_process(
                    &sid, &workspace, 20, 4,
                );
            }
            assert_eq!(wire.finish().len(), accepted_rounds);
            return;
        }
        result.unwrap();
        if no_session_case.is_some() {
            assert_eq!(
                std::fs::read(roots.path().join("start.effects")).unwrap(),
                b"actual SessionStart\n"
            );
            assert_eq!(
                std::fs::read(roots.path().join("prompt.effects")).unwrap(),
                b"actual UserPromptSubmit\n"
            );
        }
        if no_session_case == Some(true) {
            assert!(
                !crate::session::storage::session_path(&sid)
                    .unwrap()
                    .exists(),
                "explicit fresh ephemeral run must not create a saved native chain"
            );
            let summary = crate::spend::Ledger::new(&paths.state_dir)
                .summary_for_session(&sid)
                .unwrap();
            assert_eq!(summary.session.records, 1);
            assert!((summary.session.known_cost - 0.000028).abs() < 1e-12);
            assert_eq!(wire.finish().len(), 1);
            return;
        }
        // This reload precedes another request, quit, or incidental save.
        let saved = crate::session::storage::load_session_exact(&sid)
            .unwrap()
            .unwrap();
        assert_eq!(saved.native_frontier.refusal(), None);
        let history = saved
            .native_history_snapshot()
            .expect("retired native context must be durable immediately");
        assert!(
            history.host.is_some(),
            "actual built-agent host compatibility must be retained"
        );
        assert_eq!(saved.total_input_tokens, (round as u64 + 1) * 10);
        assert_eq!(saved.total_output_tokens, (round as u64 + 1) * 2);
        assert!((saved.total_cost - (round as f64 + 1.0) * 0.000028).abs() < 1e-12);
        assert!(!saved.spend_account.snapshot().tracking_error);
        let summary = crate::spend::Ledger::new(&paths.state_dir)
            .summary_for_session(&sid)
            .unwrap();
        assert_eq!(summary.session.records, round + 1);
        assert!((summary.session.known_cost - saved.total_cost).abs() < 1e-12);
        if held_end_hook {
            assert_eq!(wire.finish().len(), 1);
            return;
        }
        crate::session::lock::release_session(&sid);
        id = Some(sid);
    }
    let bodies = wire.finish();
    assert_eq!(bodies.len(), 2);
    let first_opaque = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|block| block["signature"] == "frontend preserved signature")
        .count();
    assert_eq!(
        first_opaque, 1,
        "saved native opaque block must be replayed exactly once"
    );
    assert_eq!(bodies[0]["system"], bodies[1]["system"]);
    assert_eq!(bodies[0]["tools"], bodies[1]["tools"]);
}

#[test]
fn effort46_actual_headless_frontend_on_one_mib() {
    std::thread::Builder::new()
        .name("native-headless-frontend-one-mib".into())
        .stack_size(1_048_576)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(crate::startup::heap_future(|| async {
                    native_headless_frontend_control(false, false, false, false, false, None).await;
                    native_tui_frontend_control(false, false, false, false, false).await;
                }))
        })
        .unwrap()
        .join()
        .unwrap();
}

#[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
#[tokio::test]
async fn effort46_actual_headless_frontend_reaps_held_async_session_end_before_publication() {
    native_headless_frontend_control(false, false, false, true, false, None).await;
}

#[cfg(unix)]
#[tokio::test]
async fn effort46_actual_headless_second_final_failure_refuses_separate_process_reload() {
    native_headless_frontend_control(false, false, false, false, true, None).await;
}

#[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
#[tokio::test]
async fn effort46_actual_headless_no_session_resume_refuses_before_session_start_effect() {
    native_headless_frontend_control(false, false, false, false, false, Some(false)).await;
}

#[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
#[tokio::test]
async fn effort46_actual_headless_fresh_no_session_runs_configured_start_hook_without_checkpoint() {
    native_headless_frontend_control(false, false, false, false, false, Some(true)).await;
}

#[cfg(feature = "multimodal")]
#[tokio::test]
async fn image_read64_native_exact_tool_image_tail_is_bound_and_reloadable() {
    use crate::session::{
        PersistedToolMessage,
        native_replay::{ReplayBoundary, RestartReason, RetiredNativeOutcome},
    };
    let root = crate::tests::OwnedTestDirectory::new("image64-native-tail");
    std::fs::write(
        root.path().join("pixel.png"),
        include_bytes!("agent/tools/fixtures/read-image64/3x2.png"),
    )
    .unwrap();
    let first = start()
        + &thinking(0, "", "image-signature")
        + &redacted(1)
        + &event(
            "content_block_start",
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_image64","name":"read","input":{}}}),
        )
        + &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"pixel.png\"}"}}),
        )
        + &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":2}),
        )
        + &event(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":2}}),
        )
        + &event("message_stop", json!({"type":"message_stop"}));
    let wire = Wire::new(vec![
        (true, first),
        (true, start() + &text(0, "image observed") + &finish()),
    ]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let mut tracker = crate::agent::tools::ReadTracker::new(true);
    tracker.image_support = crate::agent::tools::read_image::ImageReadSupport::captured(
        "anthropic",
        "claude-opus-5-5",
        false,
    );
    let read = crate::agent::tools::ReadTool::new_with_tracker(None, None, None, 100, tracker)
        .with_workspace(root.path());
    let mut session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    let prices = crate::print::HeadlessPricing::from_session(&session, &Config::default());
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Immutable image policy")
            .max_tokens(1000)
            .default_max_turns(2)
            .tool(read)
            .build(),
    ))
    .bind_native_turn(
        &session,
        Vec::<Message>::new().into(),
        ReplayBoundary::CompletedTurn,
        crate::provider::RecordedToolIds::Canonical,
    )
    .unwrap();
    let retry = RetryConfig {
        max_attempts: 1,
        ..Default::default()
    };
    let turn = crate::startup::heap_future(|| {
        agent.run_print_with_output(
            "inspect pixel",
            crate::print::HeadlessOutputPolicy::legacy(false, true),
            &retry,
            binding.history(),
            #[cfg(feature = "hooks")]
            None,
        )
    })
    .await;
    assert!(turn.failure.is_none(), "{:?}", turn.failure);
    assert_eq!(
        binding.commit_canonical_into_with_outcome(
            &mut session,
            "inspect pixel",
            &turn.response,
            &turn.interactions,
            turn.usage.into(),
            prices,
            RetiredNativeOutcome::Completed
        ),
        Ok(true)
    );
    assert_eq!(session.total_input_tokens, 20);
    let restored =
        crate::session::storage::decode_session_json(&serde_json::to_string(&session).unwrap())
            .unwrap();
    let native = restored
        .native_restore_history(
            crate::session::native_replay::HistoryProtocol::AnthropicMessages,
            ReplayBoundary::CompletedTurn,
        )
        .unwrap();
    assert!(
        serde_json::to_string(&native)
            .unwrap()
            .contains("image/png")
    );
    let mut missing = session.clone();
    let row=missing.messages.iter_mut().find(|row|matches!(&row.tool,Some(PersistedToolMessage::Result {images,..}) if !images.is_empty())).unwrap();
    let Some(PersistedToolMessage::Result { images, .. }) = &mut row.tool else {
        unreachable!()
    };
    images.clear();
    assert_eq!(
        binding.attach_accepted_into(
            &mut missing,
            binding.capture.as_ref().unwrap(),
            RetiredNativeOutcome::Completed
        ),
        Err(RestartReason::UnavailableSource)
    );
    let mut wrong = session.clone();
    let row=wrong.messages.iter_mut().find(|row|matches!(&row.tool,Some(PersistedToolMessage::Result {images,..}) if !images.is_empty())).unwrap();
    let Some(PersistedToolMessage::Result { images, .. }) = &mut row.tool else {
        unreachable!()
    };
    images.push(images[0].clone());
    assert_eq!(
        binding.attach_accepted_into(
            &mut wrong,
            binding.capture.as_ref().unwrap(),
            RetiredNativeOutcome::Completed
        ),
        Err(RestartReason::UnavailableSource)
    );
    let requests = wire.finish();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["system"], requests[1]["system"]);
    assert!(requests[1].to_string().contains("image-signature"));
    assert!(requests[1].to_string().contains("image/png"));
}

#[tokio::test]
async fn effort46_live_tui_changes_persist_and_preserve_signed_prefix() {
    native_tui_frontend_control(true, false, false, false, true).await;
}

#[tokio::test]
async fn effort46_actual_tui_two_turns_publish_and_reload_native_context() {
    native_tui_frontend_control(false, false, false, false, false).await;
}

#[tokio::test]
async fn effort46_actual_tui_live_app_two_turns_publish_each_boundary() {
    native_tui_frontend_control(true, false, false, false, false).await;
}

#[cfg(feature = "agent-skills")]
#[tokio::test]
async fn effort46_actual_tui_native_instruction_rebuild_generation_and_changed_body() {
    native_tui_frontend_control(true, true, false, false, false).await;
}

#[cfg(all(
    feature = "agent-skills",
    feature = "hooks",
    any(target_os = "macos", target_os = "linux")
))]
#[tokio::test]
async fn effort46_actual_tui_session_end_retires_held_hook_before_healthy_reload() {
    native_tui_frontend_control(true, false, true, false, false).await;
}

#[cfg(all(
    feature = "agent-skills",
    feature = "hooks",
    any(target_os = "macos", target_os = "linux")
))]
#[tokio::test]
async fn effort46_actual_tui_changed_policy_refuses_retired_session_end_before_effects() {
    native_tui_frontend_control(true, false, false, true, false).await;
}

async fn native_tui_frontend_control(
    same_app: bool,
    authored: bool,
    held_end: bool,
    stale_end: bool,
    effort: bool,
) {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = crate::tests::OwnedTestDirectory::new("native-tui-frontend");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "ZS_CONFIG_DIR",
            Some(roots.path().join("config").into_os_string()),
        ),
        (
            "ZS_DATA_DIR",
            Some(roots.path().join("data").into_os_string()),
        ),
        (
            "ZS_STATE_DIR",
            Some(roots.path().join("state").into_os_string()),
        ),
        (
            "ZS_CACHE_DIR",
            Some(roots.path().join("cache").into_os_string()),
        ),
    ]);
    #[cfg(feature = "hooks")]
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    #[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
    let _hook_gate = if held_end {
        let release = roots.path().join("tui-end.release");
        let gate = crate::tests::process_gate::ProcessGate::new(&release).unwrap();
        let settings = json!({"SessionEnd":[{"hooks":[
            {"type":"command","command":"/bin/sh","args":["-c", r#"printf '%s\n' "$$" > "$1"; IFS= read -r release < "$2""#, "held-tui-end", roots.path().join("tui-end.pid"), release], "trust":"trusted","async":true,"timeout":60},
            {"type":"command","command":"/bin/sh","args":["-c", r#"while [ ! -f "$1" ]; do sleep 0.01; done"#, "tui-end-observer", roots.path().join("tui-end.observed")], "trust":"trusted","timeout":15}
        ]}]});
        let config = crate::extras::hooks::settings::parse_hooks_config(&settings).unwrap();
        crate::extras::hooks::init_dispatcher(
            crate::extras::hooks::dispatcher::HookDispatcher::from_config(&config).unwrap(),
        );
        crate::extras::hooks::set_active_workspace(roots.path());
        Some(gate)
    } else if stale_end {
        let settings = json!({"SessionEnd":[{"hooks":[{"type":"command","command":"/bin/sh","args":["-c", r#"printf 'effect\n' >> "$1""#, "stale-tui-end", roots.path().join("stale-end.effects")],"trust":"trusted","timeout":5}]}]});
        let config = crate::extras::hooks::settings::parse_hooks_config(&settings).unwrap();
        crate::extras::hooks::init_dispatcher(
            crate::extras::hooks::dispatcher::HookDispatcher::from_config(&config).unwrap(),
        );
        crate::extras::hooks::set_active_workspace(roots.path());
        None
    } else {
        None
    };
    #[cfg(not(all(feature = "hooks", any(target_os = "macos", target_os = "linux"))))]
    assert!(!held_end && !stale_end);
    #[cfg(feature = "hooks")]
    struct Reset;
    #[cfg(feature = "hooks")]
    impl Drop for Reset {
        fn drop(&mut self) {
            crate::extras::hooks::reset_dispatcher();
        }
    }
    #[cfg(feature = "hooks")]
    let _reset = (held_end || stale_end).then_some(Reset);
    struct Release(String);
    impl Drop for Release {
        fn drop(&mut self) {
            crate::session::lock::release_session(&self.0);
        }
    }
    let wire = Wire::new(vec![
        (
            true,
            start()
                + &thinking(0, "", "TUI canonical signature one")
                + &(if same_app {
                    redacted(1)
                } else {
                    text(1, "first TUI answer")
                })
                + &finish(),
        ),
        (
            true,
            start()
                + &thinking(0, "", "TUI canonical signature two")
                + &text(1, "second TUI answer")
                + &finish(),
        ),
    ]);
    let mut cfg: crate::config::Config = serde_json::from_value(json!({
        "session_title_model":"off", "compact_enabled":false,
        "custom_providers":{"effort-tui":{"provider_type":"anthropic","base_url":wire.base}},
        "quick_models":{"native-test":{"provider":"effort-tui","model":"claude-opus-5-5","input_token_cost":2.0,"output_token_cost":4.0}}
    })).unwrap();
    if effort {
        cfg.reasoning = Some(crate::config::ReasoningConfig {
            effort: Some(crate::config::types::ReasoningEffort::Low),
            ..Default::default()
        });
    }
    let cli = crate::cli::Cli {
        api_key: Some("unused-native-key".into()),
        no_tools: true,
        no_context_files: true,
        no_sandbox: true,
        ..Default::default()
    };
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
    let paths = crate::paths::process_paths().unwrap();
    #[cfg(feature = "agent-skills")]
    let _immutable_cleanup =
        crate::extras::skills::session::ImmutableFixtureCleanup(roots.path().to_path_buf());
    let package = roots.path().join(".claude/skills/reviewer");
    if authored {
        let config = roots.path().join(".zerostack/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "model='native-test'\n").unwrap();
        crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
            .unwrap();
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("SKILL.md"), "---\nname: reviewer\ndescription: Review code carefully\n---\nSTABLE_NATIVE_REVIEW_GUIDANCE\n").unwrap();
    }
    let mut session = crate::session::Session::new(
        "effort-tui",
        "claude-opus-5-5",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    #[cfg(feature = "agent-skills")]
    {
        session.instruction_owner = Arc::new(
            crate::extras::skills::session::InstructionOwner::for_test(None),
        );
    }
    session.working_dir = workspace.root().to_string_lossy().into();
    session.set_input_output_prices(Some((2.0, 4.0)));
    let sid = session.id.to_string();
    let _release = Release(sid.clone());
    let mut snapshots = Vec::new();
    for (round, prompt) in ["first literal TUI prompt", "second literal TUI prompt"]
        .into_iter()
        .enumerate()
    {
        if !same_app || round == 0 {
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let client = crate::provider::create_client(
                "effort-tui",
                cli.api_key.as_deref(),
                &cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap();
            let permission = stale_end.then(|| {
                Arc::new(std::sync::Mutex::new(
                    crate::permission::checker::PermissionChecker::new(
                        &crate::permission::PermissionConfigs::default(),
                        crate::permission::SecurityMode::Standard,
                        Some(workspace.root().to_path_buf()),
                        None,
                    )
                    .unwrap(),
                ))
            });
            let ui = crate::ui::state::UiContext::new(
                &cli,
                &cfg,
                &mut session,
                &mut context,
                workspace.clone(),
                client,
                permission,
                None,
                crate::sandbox::Sandbox::new(false, "none")
                    .with_workspace_binding(workspace.clone()),
                None,
            );
            if effort {
                snapshots =
                    crate::startup::heap_future(|| crate::ui::drive_native_effort_fixture(ui))
                        .await;
            } else {
                #[cfg(feature = "agent-skills")]
                if authored {
                    snapshots = crate::startup::heap_future(|| {
                        crate::ui::drive_native_instruction_fixture(ui, &package)
                    })
                    .await;
                } else if same_app {
                    #[cfg(feature = "hooks")]
                    if stale_end {
                        snapshots = crate::startup::heap_future(|| {
                            crate::ui::drive_native_stale_session_end_fixture(ui)
                        })
                        .await;
                        assert!(!roots.path().join("stale-end.effects").exists());
                    } else {
                        let driving = crate::startup::heap_future(|| {
                            crate::ui::drive_native_fixture_turns(
                                ui,
                                &["first literal TUI prompt", "second literal TUI prompt"],
                            )
                        });
                        #[cfg(all(
                            feature = "hooks",
                            any(target_os = "macos", target_os = "linux")
                        ))]
                        if held_end {
                            use crate::tests::process_state::{ProcessIdentity, ProcessState};
                            let observer = async {
                                loop {
                                    if let Ok(text) =
                                        std::fs::read_to_string(roots.path().join("tui-end.pid"))
                                        && text.ends_with('\n')
                                        && let Ok(pid) = text.trim().parse::<u32>()
                                    {
                                        let identity = ProcessIdentity::capture(pid).unwrap();
                                        assert!(matches!(
                                            identity.state().unwrap(),
                                            ProcessState::Live { .. }
                                        ));
                                        let sessions = crate::paths::WorkspaceBinding::capture(
                                            &paths.sessions_dir(),
                                        )
                                        .unwrap();
                                        let pending =
                                            crate::session::storage::load_session_exact_read_only(
                                                &sessions, &sid, &workspace,
                                            )
                                            .unwrap()
                                            .unwrap();
                                        assert_eq!(
                                        pending.native_frontier.refusal(),
                                        Some(
                                            crate::session::native_replay::RestartReason::InFlight
                                        )
                                    );
                                        assert_eq!(
                                            (
                                                pending.total_input_tokens,
                                                pending.total_output_tokens
                                            ),
                                            (20, 4)
                                        );
                                        std::fs::write(
                                            roots.path().join("tui-end.observed"),
                                            b"observed exact hook",
                                        )
                                        .unwrap();
                                        return identity;
                                    }
                                    tokio::time::sleep(Duration::from_millis(5)).await;
                                }
                            };
                            let (completed, identity) = tokio::join!(
                                tokio::time::timeout(Duration::from_secs(5), driving),
                                tokio::time::timeout(Duration::from_secs(5), observer)
                            );
                            snapshots =
                                completed.expect("native SessionEnd outlived its owned cleanup");
                            let identity = identity.expect("configured SessionEnd did not run");
                            assert_eq!(
                                identity.state().unwrap(),
                                ProcessState::Gone,
                                "healthy native publication preceded exact hook reaping"
                            );
                            let sessions =
                                crate::paths::WorkspaceBinding::capture(&paths.sessions_dir())
                                    .unwrap();
                            let saved = crate::session::storage::load_session_exact_read_only(
                                &sessions, &sid, &workspace,
                            )
                            .unwrap()
                            .unwrap();
                            assert_eq!(saved.native_frontier.refusal(), None);
                            assert_eq!(
                                (saved.total_input_tokens, saved.total_output_tokens),
                                (20, 4)
                            );
                        } else {
                            snapshots = driving.await;
                        }
                        #[cfg(not(all(
                            feature = "hooks",
                            any(target_os = "macos", target_os = "linux")
                        )))]
                        {
                            snapshots = driving.await;
                        }
                    }
                    #[cfg(not(feature = "hooks"))]
                    {
                        snapshots = crate::startup::heap_future(|| {
                            crate::ui::drive_native_fixture_turns(
                                ui,
                                &["first literal TUI prompt", "second literal TUI prompt"],
                            )
                        })
                        .await;
                    }
                } else {
                    crate::startup::heap_future(|| {
                        crate::ui::drive_native_fixture_turn(ui, prompt)
                    })
                    .await;
                    let sessions =
                        crate::paths::WorkspaceBinding::capture(&paths.sessions_dir()).unwrap();
                    snapshots = vec![
                        crate::session::storage::load_session_exact_read_only(
                            &sessions, &sid, &workspace,
                        )
                        .unwrap()
                        .unwrap(),
                    ];
                }
                #[cfg(not(feature = "agent-skills"))]
                {
                    assert!(!authored && !held_end && !stale_end);
                    if same_app {
                        snapshots = crate::startup::heap_future(|| {
                            crate::ui::drive_native_fixture_turns(
                                ui,
                                &["first literal TUI prompt", "second literal TUI prompt"],
                            )
                        })
                        .await;
                    } else {
                        crate::startup::heap_future(|| {
                            crate::ui::drive_native_fixture_turn(ui, prompt)
                        })
                        .await;
                        let sessions =
                            crate::paths::WorkspaceBinding::capture(&paths.sessions_dir()).unwrap();
                        snapshots = vec![
                            crate::session::storage::load_session_exact_read_only(
                                &sessions, &sid, &workspace,
                            )
                            .unwrap()
                            .unwrap(),
                        ];
                    }
                }
            }
        }
        let saved = snapshots[if same_app { round } else { 0 }].clone();
        assert_eq!(saved.native_frontier.refusal(), None);
        assert!(saved.native_history_snapshot().unwrap().host.is_some());
        assert_eq!(
            (saved.total_input_tokens, saved.total_output_tokens),
            ((round as u64 + 1) * 10, (round as u64 + 1) * 2)
        );
        assert_eq!(
            saved
                .messages
                .iter()
                .filter(|row| row.role == crate::session::MessageRole::User)
                .count(),
            round + 1
        );
        let summary = crate::spend::Ledger::new(&paths.state_dir)
            .summary_for_session(&sid)
            .unwrap();
        let paid_rounds = if same_app { 2 } else { round + 1 };
        assert_eq!(summary.session.records, paid_rounds);
        assert!((summary.session.known_cost - paid_rounds as f64 * 0.000028).abs() < 1e-12);
        if !same_app {
            crate::session::lock::release_session(&sid);
            session = saved;
        }
    }
    let bodies = wire.finish();
    assert_eq!(bodies.len(), 2);
    if effort {
        assert_eq!(bodies[0]["output_config"]["effort"], "low");
        assert_eq!(bodies[1]["output_config"]["effort"], "max");
        assert_eq!(bodies[0]["thinking"], json!({"type":"adaptive"}));
        assert_eq!(bodies[1]["thinking"], json!({"type":"adaptive"}));
    }

    assert_eq!(bodies[0]["system"], bodies[1]["system"]);
    if authored {
        assert!(
            bodies[0]["system"]
                .to_string()
                .contains("STABLE_NATIVE_REVIEW_GUIDANCE")
        );
        assert!(
            !bodies
                .iter()
                .any(|body| body.to_string().contains("CHANGED_NATIVE_REVIEW_GUIDANCE"))
        );
    }
    assert_eq!(bodies[0]["tools"], bodies[1]["tools"]);
    let replayed = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|block| block["signature"] == "TUI canonical signature one")
        .count();
    assert_eq!(replayed, 1);
}

#[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
#[tokio::test]
async fn effort46_actual_session_start_context_resumes_saved_native_prefix() {
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = crate::tests::OwnedTestDirectory::new("native-startup-context-resume");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "ZS_CONFIG_DIR",
            Some(roots.path().join("config").into_os_string()),
        ),
        (
            "ZS_DATA_DIR",
            Some(roots.path().join("data").into_os_string()),
        ),
        (
            "ZS_STATE_DIR",
            Some(roots.path().join("state").into_os_string()),
        ),
        (
            "ZS_CACHE_DIR",
            Some(roots.path().join("cache").into_os_string()),
        ),
    ]);
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            crate::extras::hooks::reset_dispatcher();
        }
    }
    let _reset = Reset;
    let notice = json!({"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"OWNED_STARTUP_CONTEXT_雪"}}).to_string();
    let settings = json!({"hooks":{"SessionStart":[{"hooks":[{
        "type":"command", "command":"/bin/sh", "trust":"trusted", "timeout":5,
        "args":["-c",r#"printf 'start\n' >> "$2"; printf '%s\n' "$1""#,"startup-context",notice,roots.path().join("hook.effects")]
    }]}]}});
    crate::fs::private_atomic_write_sync(
        &roots.path().join("config/settings.json"),
        serde_json::to_vec(&settings).unwrap().as_slice(),
    )
    .unwrap();
    let hooks = crate::extras::hooks::settings::parse_hooks_config(&settings["hooks"]).unwrap();
    crate::extras::hooks::init_dispatcher(
        crate::extras::hooks::dispatcher::HookDispatcher::from_config(&hooks).unwrap(),
    );
    let wire = Wire::new(vec![
        (
            true,
            start()
                + &thinking(0, "", "startup-first-signature")
                + &text(1, "first owned startup answer")
                + &finish(),
        ),
        (
            true,
            start()
                + &thinking(0, "", "startup-second-signature")
                + &text(1, "second owned startup answer")
                + &finish(),
        ),
    ]);
    let cfg:crate::config::Config=serde_json::from_value(json!({
        "session_title_model":"off",
        "custom_providers":{"native-startup":{"provider_type":"anthropic","base_url":wire.base}},
        "quick_models":{"startup-test":{"provider":"native-startup","model":"claude-opus-5-5","input_token_cost":2.0,"output_token_cost":4.0}}
    })).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
    let paths = crate::paths::process_paths().unwrap();
    struct Release(Option<String>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(id) = &self.0 {
                crate::session::lock::release_session(id);
            }
        }
    }
    let mut release = Release(None);
    let mut id = None;
    for (round, prompt) in [
        "first literal startup prompt",
        "second literal startup prompt",
    ]
    .into_iter()
    .enumerate()
    {
        let cli = crate::cli::Cli {
            api_key: Some("unused-native-key".into()),
            quick_model: Some("startup-test".into()),
            session: id.clone(),
            print: true,
            quiet: true,
            no_context_files: true,
            no_tools: true,
            no_sandbox: true,
            message: vec![prompt.into()],
            ..Default::default()
        };
        let mut startup = crate::startup::heap_future(|| async {
            Box::new(
                crate::startup::Startup::init(
                    cli,
                    cfg.clone(),
                    paths.clone(),
                    workspace.clone(),
                    false,
                    false,
                    false,
                )
                .await
                .unwrap(),
            )
        })
        .await;
        let sid = startup.session.id.to_string();
        release.0 = Some(sid.clone());
        startup.init_features().await.unwrap();
        startup.resolve_prompts().await.unwrap();
        crate::startup::heap_future(|| startup.dispatch())
            .await
            .unwrap();
        let sessions_root = crate::paths::WorkspaceBinding::capture(&paths.sessions_dir()).unwrap();
        let saved =
            crate::session::storage::load_session_exact_read_only(&sessions_root, &sid, &workspace)
                .unwrap()
                .unwrap();
        assert_eq!(saved.native_frontier.refusal(), None);
        assert!(saved.native_publication.is_none());
        assert_eq!(
            (saved.total_input_tokens, saved.total_output_tokens),
            ((round as u64 + 1) * 10, (round as u64 + 1) * 2)
        );
        assert_eq!(
            saved
                .messages
                .iter()
                .filter(|row| row.role == crate::session::MessageRole::System
                    && row.content.contains("OWNED_STARTUP_CONTEXT_雪"))
                .count(),
            round + 1
        );
        assert_eq!(
            saved
                .messages
                .iter()
                .filter(|row| row.role == crate::session::MessageRole::User)
                .count(),
            round + 1
        );
        assert_eq!(
            std::fs::read_to_string(roots.path().join("hook.effects")).unwrap(),
            "start\n".repeat(round + 1)
        );
        let ledger = crate::spend::Ledger::new(&paths.state_dir)
            .summary_for_session(&sid)
            .unwrap();
        assert_eq!(ledger.session.records, round + 1);
        assert!((ledger.session.known_cost - (round as f64 + 1.0) * 0.000028).abs() < 1e-12);
        crate::session::lock::release_session(&sid);
        id = Some(sid);
    }
    let bodies = wire.finish();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["system"], bodies[1]["system"]);
    assert_eq!(bodies[0]["tools"], bodies[1]["tools"]);
    let messages = serde_json::to_string(&bodies[1]["messages"]).unwrap();
    assert_eq!(messages.matches("startup-first-signature").count(), 1);
    assert_eq!(messages.matches("OWNED_STARTUP_CONTEXT_雪").count(), 2);
    assert!(
        messages.contains("first literal startup prompt")
            && messages.contains("second literal startup prompt")
    );
}

#[tokio::test]
async fn steering79_actual_native_tui_injects_canonical_input_and_reloads() {
    native_steering79_case(false).await;
}

#[tokio::test]
async fn steering79_native_context_full_preserves_pending_draft_without_fake_completion() {
    native_steering79_case(true).await;
}

async fn native_steering79_case(stop_at_boundary: bool) {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = crate::tests::OwnedTestDirectory::new("native-steering79");
    let _environment = crate::tests::ScopedProcessEnv::set(
        &[
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ]
        .map(|name| (name, Some(roots.path().join(name).into_os_string()))),
    );
    let first = start()
        + &thinking(0, "", "STEERING79_SIGNATURE")
        + &redacted(1)
        + &event(
            "content_block_start",
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_steer79","name":"read","input":{}}}),
        )
        + &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"held.txt\"}"}}),
        )
        + &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":2}),
        )
        + &event(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":2}}),
        )
        + &event("message_stop", json!({"type":"message_stop"}));
    let wire = Wire::new(if stop_at_boundary {
        vec![(true, first)]
    } else {
        vec![
            (true, first),
            (
                true,
                start() + &text(0, "steered native answer") + &finish(),
            ),
        ]
    });
    let mut cfg: crate::config::Config = serde_json::from_value(json!({"session_title_model":"off","compact_enabled":false,"custom_providers":{"steer79":{"provider_type":"anthropic","base_url":wire.base}},"quick_models":{"native-test":{"provider":"steer79","model":"claude-opus-5-5","input_token_cost":2.0,"output_token_cost":4.0}}})).unwrap();
    if stop_at_boundary {
        cfg.compact_enabled = Some(true);
        cfg.mid_turn_compact_threshold = Some(0.00001);
    }
    let cli = crate::cli::Cli {
        api_key: Some("unused-native-key".into()),
        tools: vec!["read".into()],
        no_context_files: true,
        no_sandbox: true,
        ..Default::default()
    };
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
    std::fs::write(roots.path().join("held.txt"), "steering owned read").unwrap();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = crate::session::Session::new(
        "steer79",
        "claude-opus-5-5",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    session.working_dir = workspace.root().to_string_lossy().into();
    session.set_input_output_prices(Some((2.0, 4.0)));
    let sid = session.id.to_string();
    let client = crate::provider::create_client(
        "steer79",
        cli.api_key.as_deref(),
        &cfg.custom_providers_map(),
        cfg.api_keys.as_ref(),
    )
    .unwrap();
    let ui = crate::ui::state::UiContext::new(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace.clone(),
        client,
        None,
        None,
        crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace.clone()),
        None,
    );
    let saved =
        crate::startup::heap_future(|| crate::ui::drive_native_steer_fixture(ui, stop_at_boundary))
            .await;
    crate::session::lock::release_session(&sid);
    assert_eq!(saved.native_frontier.refusal(), None);
    assert_eq!(
        saved
            .native_history_snapshot()
            .unwrap()
            .saved_boundary()
            .unwrap(),
        if stop_at_boundary {
            crate::session::native_replay::ReplayBoundary::LiveToolReply
        } else {
            crate::session::native_replay::ReplayBoundary::CompletedTurn
        }
    );
    if stop_at_boundary {
        assert!(
            saved
                .native_history_snapshot()
                .unwrap()
                .require_completed_boundary()
                .is_err()
        );
    }
    assert_eq!(
        (saved.total_input_tokens, saved.total_output_tokens),
        if stop_at_boundary { (10, 2) } else { (20, 4) }
    );
    assert_eq!(
        saved
            .messages
            .iter()
            .filter(|row| row.role == crate::session::MessageRole::User
                && row.content.as_str() == "redirect the running task")
            .count(),
        usize::from(!stop_at_boundary)
    );
    let bodies = wire.finish();
    assert_eq!(bodies.len(), if stop_at_boundary { 1 } else { 2 });
    assert!(!bodies[0].to_string().contains("redirect the running task"));
    if !stop_at_boundary {
        assert!(bodies[1].to_string().contains("redirect the running task"));
        assert_eq!(
            bodies[1]
                .to_string()
                .matches("STEERING79_SIGNATURE")
                .count(),
            1
        );
    }
    assert_eq!(
        crate::spend::Ledger::new(&crate::paths::process_paths().unwrap().state_dir)
            .summary_for_session(&sid)
            .unwrap()
            .session
            .records,
        if stop_at_boundary { 1 } else { 2 }
    );
}

#[tokio::test]
async fn effort46_live_headless_resume_uses_persisted_main_override() {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = crate::tests::OwnedTestDirectory::new("effort-live-headless");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "ZS_CONFIG_DIR",
            Some(roots.path().join("config").into_os_string()),
        ),
        (
            "ZS_DATA_DIR",
            Some(roots.path().join("data").into_os_string()),
        ),
        (
            "ZS_STATE_DIR",
            Some(roots.path().join("state").into_os_string()),
        ),
        (
            "ZS_CACHE_DIR",
            Some(roots.path().join("cache").into_os_string()),
        ),
    ]);
    #[cfg(feature = "hooks")]
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    let wire = Wire::new(vec![
        (
            true,
            start()
                + &thinking(0, "", "effort-headless-signature-one")
                + &text(1, "first effort answer")
                + &finish(),
        ),
        (
            true,
            start()
                + &thinking(0, "", "effort-headless-signature-two")
                + &text(1, "second effort answer")
                + &finish(),
        ),
    ]);
    let cfg:crate::config::Config=serde_json::from_value(json!({
        "session_title_model":"off","compact_enabled":false,
        "reasoning":{"effort":"low"},
        "custom_providers":{"effort-headless":{"provider_type":"anthropic","base_url":wire.base}},
        "quick_models":{"effort-model":{"provider":"effort-headless","model":"claude-opus-5-5","input_token_cost":2.0,"output_token_cost":4.0}}
    })).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
    let paths = crate::paths::process_paths().unwrap();
    let mut seed = crate::session::Session::new("effort-headless", "claude-opus-5-5", 128_000, "");
    seed.working_dir = workspace.root().to_string_lossy().into_owned().into();
    seed.reasoning_controls.effort = Some(crate::config::types::ReasoningEffort::Max);
    seed.set_input_output_prices(Some((2.0, 4.0)));
    let sid = seed.id.to_string();
    struct Release(String);
    impl Drop for Release {
        fn drop(&mut self) {
            crate::session::lock::release_session(&self.0);
        }
    }
    let _release = Release(sid.clone());
    crate::session::storage::save_session(&seed).unwrap();
    crate::session::lock::release_session(&sid);
    for (index, prompt) in [
        "first literal effort prompt",
        "second literal effort prompt",
    ]
    .into_iter()
    .enumerate()
    {
        let cli = crate::cli::Cli {
            api_key: Some("owned-effort-key".into()),
            quick_model: Some("effort-model".into()),
            session: Some(sid.clone()),
            print: true,
            quiet: true,
            no_context_files: true,
            no_tools: true,
            no_sandbox: true,
            message: vec![prompt.into()],
            ..Default::default()
        };
        let mut startup = crate::startup::heap_future(|| async {
            Box::new(
                crate::startup::Startup::init(
                    cli,
                    cfg.clone(),
                    paths.clone(),
                    workspace.clone(),
                    false,
                    false,
                    false,
                )
                .await
                .unwrap(),
            )
        })
        .await;
        crate::startup::heap_future(|| startup.init_features())
            .await
            .unwrap();
        crate::startup::heap_future(|| startup.resolve_prompts())
            .await
            .unwrap();
        crate::startup::heap_future(|| startup.dispatch())
            .await
            .unwrap();
        let sessions = crate::paths::WorkspaceBinding::capture(&paths.sessions_dir()).unwrap();
        let restored =
            crate::session::storage::load_session_exact_read_only(&sessions, &sid, &workspace)
                .unwrap()
                .unwrap();
        assert_eq!(
            restored.reasoning_controls.effort,
            Some(crate::config::types::ReasoningEffort::Max)
        );
        assert_eq!(restored.native_frontier.refusal(), None);
        assert_eq!(
            (restored.total_input_tokens, restored.total_output_tokens),
            ((index as u64 + 1) * 10, (index as u64 + 1) * 2)
        );
        assert_eq!(
            crate::spend::Ledger::new(&paths.state_dir)
                .summary_for_session(&sid)
                .unwrap()
                .session
                .records,
            index + 1
        );
        crate::session::lock::release_session(&sid);
    }
    let bodies = wire.finish();
    assert_eq!(bodies.len(), 2);
    for body in &bodies {
        assert_eq!(body["output_config"]["effort"], "max");
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
    }
    assert_eq!(bodies[0]["system"], bodies[1]["system"]);
    assert_eq!(bodies[0]["tools"], bodies[1]["tools"]);
    assert_eq!(
        bodies[1]["messages"]
            .to_string()
            .matches("effort-headless-signature-one")
            .count(),
        1
    );
}

#[tokio::test]
async fn effort46_live_gateway_uses_gateway_reasoning_and_native_unsupported_has_zero_http() {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let _environment = crate::tests::ScopedProcessEnv::set(&[("RUST_LOG", Some("off".into()))]);
    let chunk = |delta: Value, finish: Option<&str>, usage: Option<Value>| {
        let mut value = json!({"id":"effort-gateway","object":"chat.completion.chunk","created":0,"model":"anthropic/claude-opus-5.5","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        if let Some(usage) = usage {
            value["usage"] = usage;
        }
        format!("data: {value}\n\n")
    };
    let reply = chunk(
        json!({"role":"assistant","content":"gateway effort done"}),
        None,
        None,
    ) + &chunk(
        json!({}),
        Some("stop"),
        Some(json!({"prompt_tokens":10,"completion_tokens":2,"total_tokens":12})),
    ) + "data: [DONE]\n\n";
    let wire = Wire::at_path(vec![(true, reply)], "/chat/completions");
    let AnyModel::OpenRouter(model, routing) = wire
        .client_for("openrouter")
        .completion_model("anthropic/claude-opus-5.5")
    else {
        unreachable!()
    };
    let cfg:crate::config::Config=serde_json::from_value(json!({"reasoning":{"effort":"high"},"extra_body":{"reasoning":{"effort":"low","max_tokens":2000}}})).unwrap();
    let agent = AnyAgent::without_skills(AnyAgentInner::OpenRouter(
        rig::agent::AgentBuilder::new(model)
            .preamble("static gateway prefix")
            .max_tokens(64)
            .additional_params(merge_extra_body(routing, cfg.extra_body.clone()).unwrap())
            .build(),
    ))
    .with_reasoning_controls(
        &cfg,
        effort::SessionReasoning {
            effort: Some(crate::config::types::ReasoningEffort::Max),
            thinking: None,
        },
    );
    let turn = agent
        .run_print(
            "literal gateway prompt",
            true,
            false,
            &cfg.retry,
            Vec::<Message>::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(turn.failure.is_none(), "{:?}", turn.failure);
    assert_eq!(turn.response, "gateway effort done");
    let bodies = wire.finish();
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0]["reasoning"],
        json!({"effort":"max","enabled":true})
    );
    assert!(bodies[0].get("thinking").is_none());
    assert!(bodies[0].get("output_config").is_none());
    let negative = Wire::new(Vec::new());
    let AnyModel::Anthropic(model) = negative.client().completion_model("claude-opus-4-6") else {
        unreachable!()
    };
    let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("unchanged prefix")
            .max_tokens(64)
            .build(),
    ))
    .with_reasoning_controls(
        &cfg,
        effort::SessionReasoning {
            effort: Some(crate::config::types::ReasoningEffort::Xhigh),
            thinking: None,
        },
    );
    let failed = agent
        .run_print(
            "must not dispatch",
            true,
            false,
            &cfg.retry,
            Vec::<Message>::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(failed.failure.is_some());
    assert!(
        failed
            .failure
            .unwrap()
            .to_string()
            .contains("xhigh effort is not supported")
    );
    assert!(failed.interactions.is_empty());
    assert!(negative.finish().is_empty());
}

#[cfg(feature = "memory")]
async fn cache71_memory_native_roundtrip(legacy: bool) {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = crate::tests::OwnedTestDirectory::new("cache71-memory-native");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "ZS_CONFIG_DIR",
            Some(roots.path().join("config").into_os_string()),
        ),
        (
            "ZS_DATA_DIR",
            Some(roots.path().join("data").into_os_string()),
        ),
        (
            "ZS_STATE_DIR",
            Some(roots.path().join("state").into_os_string()),
        ),
        (
            "ZS_CACHE_DIR",
            Some(roots.path().join("cache").into_os_string()),
        ),
    ]);
    #[cfg(feature = "hooks")]
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    let mut responses = vec![
        (
            true,
            start()
                + &thinking(0, "", "cache71-memory-signature-one")
                + &text(1, "first effort answer")
                + &finish(),
        ),
        (
            true,
            start()
                + &thinking(0, "", "cache71-memory-signature-two")
                + &text(1, "second effort answer")
                + &finish(),
        ),
    ];
    if !legacy {
        responses.push((
            true,
            start() + &text(0, "third unchanged memory answer") + &finish(),
        ));
    }
    let wire = Wire::new(responses);
    let cfg:crate::config::Config=serde_json::from_value(json!({
        "session_title_model":"off","compact_enabled":false,
        "cache_ttl":"1h",
        "custom_providers":{"cache71-memory":{"provider_type":"anthropic","base_url":wire.base}},
        "quick_models":{"cache71-model":{"provider":"cache71-memory","model":"claude-opus-5-5","input_token_cost":2.0,"output_token_cost":4.0}}
    })).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
    let paths = crate::paths::process_paths().unwrap();
    let mut seed = crate::session::Session::new("cache71-memory", "claude-opus-5-5", 128_000, "");
    seed.working_dir = workspace.root().to_string_lossy().into_owned().into();
    if legacy {
        seed.memory_placement = crate::session::MemoryPlacement::LegacySystem;
    }
    seed.set_input_output_prices(Some((2.0, 4.0)));
    let sid = seed.id.to_string();
    struct Release(String);
    impl Drop for Release {
        fn drop(&mut self) {
            crate::session::lock::release_session(&self.0);
        }
    }
    let _release = Release(sid.clone());
    crate::session::storage::save_session(&seed).unwrap();
    crate::session::lock::release_session(&sid);
    let prompts = if legacy {
        vec![
            "first literal memory prompt",
            "second literal memory prompt",
        ]
    } else {
        vec![
            "first literal memory prompt",
            "second literal memory prompt",
            "third literal memory prompt",
        ]
    };
    let memory_file = crate::extras::memory::Mem::open().memory_md();
    crate::fs::ensure_private_directory(memory_file.parent().unwrap()).unwrap();
    for (index, prompt) in prompts.iter().enumerate() {
        std::fs::write(
            &memory_file,
            if legacy || index == 0 {
                "CACHE_MEMORY_A_SENTINEL"
            } else {
                "CACHE_MEMORY_B_SENTINEL"
            },
        )
        .unwrap();
        let cli = crate::cli::Cli {
            api_key: Some("owned-effort-key".into()),
            quick_model: Some("cache71-model".into()),
            session: Some(sid.clone()),
            print: true,
            quiet: true,
            no_context_files: true,
            no_tools: true,
            no_sandbox: true,
            message: vec![(*prompt).into()],
            ..Default::default()
        };
        let mut startup = crate::startup::heap_future(|| async {
            Box::new(
                crate::startup::Startup::init(
                    cli,
                    cfg.clone(),
                    paths.clone(),
                    workspace.clone(),
                    false,
                    false,
                    false,
                )
                .await
                .unwrap(),
            )
        })
        .await;
        crate::startup::heap_future(|| startup.init_features())
            .await
            .unwrap();
        crate::startup::heap_future(|| startup.resolve_prompts())
            .await
            .unwrap();
        crate::startup::heap_future(|| startup.dispatch())
            .await
            .unwrap();
        let sessions = crate::paths::WorkspaceBinding::capture(&paths.sessions_dir()).unwrap();
        let restored =
            crate::session::storage::load_session_exact_read_only(&sessions, &sid, &workspace)
                .unwrap()
                .unwrap();
        assert_eq!(
            restored.memory_placement,
            if legacy {
                crate::session::MemoryPlacement::LegacySystem
            } else {
                crate::session::MemoryPlacement::Messages
            }
        );
        let notices: Vec<_> = restored
            .messages
            .iter()
            .filter(|row| {
                row.role == crate::session::MessageRole::System
                    && row.content.starts_with("Persistent memory snapshot")
            })
            .collect();
        assert_eq!(notices.len(), if legacy { 0 } else { (index + 1).min(2) });
        if !legacy {
            assert!(notices[0].content.contains("CACHE_MEMORY_A_SENTINEL"));
            if index > 0 {
                assert!(notices[1].content.contains("CACHE_MEMORY_B_SENTINEL"));
            }
        }
        assert_eq!(restored.native_frontier.refusal(), None);
        assert_eq!(
            (restored.total_input_tokens, restored.total_output_tokens),
            ((index as u64 + 1) * 10, (index as u64 + 1) * 2)
        );
        assert_eq!(
            crate::spend::Ledger::new(&paths.state_dir)
                .summary_for_session(&sid)
                .unwrap()
                .session
                .records,
            index + 1
        );
        crate::session::lock::release_session(&sid);
    }
    let bodies = wire.finish();
    assert_eq!(bodies.len(), if legacy { 2 } else { 3 });
    for body in &bodies {
        assert_eq!(body["system"], bodies[0]["system"]);
        assert_eq!(body["tools"], bodies[0]["tools"]);
        let system = body["system"].to_string();
        assert_eq!(system.contains("CACHE_MEMORY_A_SENTINEL"), legacy);
        assert!(!system.contains("CACHE_MEMORY_B_SENTINEL"));
        assert_eq!(
            body["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        assert_eq!(
            body["system"].as_array().unwrap().last().unwrap()["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"}),
            "the static prefix has its own breakpoint before memory messages"
        );
    }
    assert_eq!(
        bodies[1]["messages"]
            .to_string()
            .matches("cache71-memory-signature-one")
            .count(),
        1
    );
    if !legacy {
        for body in &bodies[1..] {
            let messages = body["messages"].to_string();
            assert_eq!(messages.matches("CACHE_MEMORY_A_SENTINEL").count(), 1);
            assert_eq!(messages.matches("CACHE_MEMORY_B_SENTINEL").count(), 1);
        }
    }
}

#[cfg(feature = "memory")]
#[tokio::test]
async fn cache71_memory_native_updates_keep_cached_prefix_and_saved_signed_history() {
    cache71_memory_native_roundtrip(false).await;
}

#[cfg(feature = "memory")]
#[tokio::test]
async fn cache71_memory_legacy_native_resume_retains_its_signed_system_layout() {
    cache71_memory_native_roundtrip(true).await;
}

#[cfg(feature = "memory")]
#[tokio::test]
async fn cache71_memory_actual_paused_write_checkpoint_tracks_the_literal_user_after_context() {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let root = crate::tests::OwnedTestDirectory::new("cache71-memory-write-anchor");
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    std::fs::write(root.path().join("file.txt"), "ORIGINAL_MEMORY_PREIMAGE").unwrap();
    let first = start()
        + &thinking(0, "", "memory-write-signature")
        + &event(
            "content_block_start",
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"memory-write","name":"write","input":{}}}),
        )
        + &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"file.txt\",\"content\":\"AFTER_MEMORY_WRITE\",\"overwrite\":true}"}}),
        )
        + &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":1}),
        )
        + &event(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":2}}),
        )
        + &event("message_stop", json!({"type":"message_stop"}));
    let wire = Wire::new(vec![
        (true, first),
        (true, start() + &text(0, "memory write done") + &finish()),
    ]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let preamble = "STATIC\n\n---\n\nMEMORY_ANCHOR_SENTINEL".to_string();
    let snapshot = crate::agent::builder::PreambleSnapshot {
        tokens: crate::session::Session::estimate_tokens(&preamble),
        bytes: preamble.len(),
        parts: Vec::new(),
        ranges: vec![("Static", 0..6), ("Memory", 6..preamble.len())],
        instruction_sources: None,
    };
    use rig::tool::Tool;
    let tracker = crate::agent::tools::ReadTracker::new(true);
    crate::agent::tools::read::ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
        .with_workspace_binding(workspace.clone())
        .call(crate::agent::tools::ReadArgs {
            raw_notebook: false,
            path: "file.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
    let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble(&preamble)
            .max_tokens(128)
            .default_max_turns(2)
            .tool(
                crate::agent::tools::write::WriteTool::new_with_tracker(None, None, None, tracker)
                    .with_workspace_binding(workspace.clone()),
            )
            .build(),
    ))
    .with_context_snapshot(Some(snapshot));
    let mut session = crate::session::Session::new("native-memory", "claude-opus-5-5", 128_000, "");
    let agent = agent
        .with_main_session(&crate::config::Config::default(), &session)
        .with_file_checkpoints(session.file_checkpoints.recorder(&session.id, 0, true));
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let mut paused = crate::startup::heap_future(|| {
        agent.spawn_runner_paused_in_scope(
            "literal memory write".into(),
            Vec::<Message>::new(),
            RetryConfig::default(),
            #[cfg(feature = "hooks")]
            None,
            scope.clone(),
        )
    })
    .await;
    let mut pending = crate::ui::state::PendingMainTurn::capture(&session, "literal memory write");
    crate::ui::adopt_runner_hook_context(&mut paused, Some(&mut pending), &mut session);
    let mut run = crate::ui::state::AgentRunState::default();
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending);
    assert!(
        !run.pending_turn
            .as_ref()
            .unwrap()
            .has_progress(&session, "", &[]),
        "context alone is not accepted model/file progress"
    );
    let user = session
        .messages
        .iter()
        .position(|row| row.role == crate::session::MessageRole::User)
        .unwrap();
    assert_eq!(user, 1);
    let mut runner = paused.start_interactive();
    let mut done = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Done { .. } => {
                    done = true;
                    break;
                }
                crate::event::AgentEvent::Error { message, .. } => panic!("{message}"),
                crate::event::AgentEvent::UsageDelta {
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
    .unwrap();
    cleanup.settle().await;
    assert!(done);
    assert_eq!(
        std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
        "AFTER_MEMORY_WRITE"
    );
    let records = serde_json::to_value(&session.file_checkpoints).unwrap();
    assert_eq!(records["records"].as_array().unwrap().len(), 1);
    assert_eq!(records["records"][0]["turn"], user);
    assert!(
        run.pending_turn
            .as_ref()
            .unwrap()
            .has_progress(&session, "", &[])
    );
    assert_eq!(
        session
            .file_checkpoints
            .restore(&session.id, user + 1, &workspace)
            .unwrap(),
        0
    );
    assert_eq!(
        session
            .file_checkpoints
            .restore(&session.id, user, &workspace)
            .unwrap(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
        "ORIGINAL_MEMORY_PREIMAGE"
    );
    let bodies = wire.finish();
    assert_eq!(bodies.len(), 2);
    assert!(
        !bodies[0]["system"]
            .to_string()
            .contains("MEMORY_ANCHOR_SENTINEL")
    );
    assert!(
        bodies[0]["messages"]
            .to_string()
            .contains("MEMORY_ANCHOR_SENTINEL")
    );
}
