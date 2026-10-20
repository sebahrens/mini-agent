//! Native local HTTP witnesses for auxiliary receipts, failure, and cancellation.
use super::run::{Run, UsageObserver};
use super::{Account, Ledger, PriceSnapshot, RequestKind};
use crate::{
    config::Config,
    provider::AnyClient,
    session::{MessageRole, SessionMessage},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Wire {
    address: std::net::SocketAddr,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    changed: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
    #[cfg(feature = "subagents")]
    release: Arc<tokio::sync::Notify>,
}
impl Drop for Wire {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Wire {
    async fn new(responses: Vec<Option<String>>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let changed = Arc::new(tokio::sync::Notify::new());
        let (seen, captured, notify) = (calls.clone(), bodies.clone(), changed.clone());
        let release = Arc::new(tokio::sync::Notify::new());
        let release_server = release.clone();
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            for response in responses {
                let (mut socket, _) =
                    tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let body = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    let mut bytes = Vec::new();
                    let header_end = loop {
                        let mut chunk = [0; 1024];
                        let count = socket.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&chunk[..count]);
                        assert!(bytes.len() <= 16 * 1024);
                        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            break end + 4;
                        }
                    };
                    let length = std::str::from_utf8(&bytes[..header_end])
                        .unwrap()
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        })
                        .unwrap();
                    assert!(length <= 128 * 1024);
                    while bytes.len() < header_end + length {
                        let mut chunk = [0; 1024];
                        let count = socket.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    serde_json::from_slice::<serde_json::Value>(
                        &bytes[header_end..header_end + length],
                    )
                    .unwrap()
                })
                .await
                .unwrap();
                captured.lock().unwrap().push(body);
                seen.fetch_add(1, Ordering::AcqRel);
                notify.notify_one();
                if let Some(response) = response {
                    let response = if let Some(response) = response.strip_prefix("HOLD_GATE\n") {
                        release_server.notified().await;
                        response.to_owned()
                    } else {
                        response
                    };
                    if let Some(response) = response.strip_prefix("HOLD_PAIR\n") {
                        let (text, tail) = response.split_once("\n\n").unwrap();
                        let text = format!("{text}\n\n");
                        let headers = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{text}\r\n",
                            text.len()
                        );
                        socket.write_all(headers.as_bytes()).await.unwrap();
                        held.push((socket, tail.to_owned()));
                        if held.len() == 2 {
                            release_server.notified().await;
                            for (mut socket, tail) in held.drain(..) {
                                socket
                                    .write_all(
                                        format!("{:x}\r\n{tail}\r\n0\r\n\r\n", tail.len())
                                            .as_bytes(),
                                    )
                                    .await
                                    .unwrap();
                                socket.shutdown().await.unwrap();
                            }
                        }
                        continue;
                    }
                    let message = if response.starts_with("HTTP/1.1 ") {
                        response
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                            response.len()
                        )
                    };
                    socket.write_all(message.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                } else {
                    let chunk = response_text_chunk();
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{chunk}\r\n",
                        chunk.len()
                    );
                    socket.write_all(headers.as_bytes()).await.unwrap();
                    std::future::pending::<()>().await;
                }
            }
        });
        Self {
            address,
            calls,
            bodies,
            changed,
            task,
            #[cfg(feature = "subagents")]
            release,
        }
    }
    async fn wait_calls(&self, expected: usize) {
        // Event-driven on `changed`; the bound only turns a lost request into a
        // failure and is generous so a loaded parallel run never trips it.
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while self.calls.load(Ordering::Acquire) < expected {
                self.changed.notified().await;
            }
        })
        .await
        .unwrap();
    }
    fn client(&self) -> (AnyClient, Config) {
        let cfg: Config = serde_json::from_value(serde_json::json!({"retry":{"max_attempts":1},"custom_providers":{"cost-wire":{"provider_type":"openai","base_url":format!("http://{}/v1",self.address),"api_style":"completions"}}})).unwrap();
        let client = crate::provider::create_client(
            "cost-wire",
            Some("unused-fixture-key"),
            cfg.custom_providers_map(),
            None,
        )
        .unwrap();
        (client, cfg)
    }
}
fn response_text_chunk() -> String {
    "data: {\"id\":\"held\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"pending\"},\"finish_reason\":null}]}\n\n".into()
}
fn response(text: &str, tool: bool) -> String {
    let delta = if tool {
        serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"never-call","type":"function","function":{"name":"write","arguments":"{\"path\":\"NEVER\",\"content\":\"not-authorized\"}"}}]})
    } else {
        serde_json::json!({"role":"assistant","content":text})
    };
    let chunk = serde_json::json!({"id":"cost-wire","object":"chat.completion.chunk","created":0,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let final_chunk = serde_json::json!({"id":"cost-wire","object":"chat.completion.chunk","created":0,"model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":if tool {"tool_calls"}else{"stop"}}],"usage":{"prompt_tokens":50,"completion_tokens":10,"total_tokens":60}});
    format!("data: {chunk}\n\ndata: {final_chunk}\n\ndata: [DONE]\n\n")
}
fn profile(
    root: &crate::tests::OwnedTestDirectory,
    cfg: &Config,
    kind: RequestKind,
    cap: Option<f64>,
) -> (Run, Arc<Ledger>) {
    let ledger = Arc::new(Ledger::new(root.path()));
    let account = Account::new("native-cost-session", cap, 0.0).unwrap();
    let run = Run::fixture(
        account,
        ledger.clone(),
        "native-cost-session",
        PriceSnapshot::capture(Some((1.0, 2.0)), Default::default(), false),
    );
    (
        run.with_captured_profile("cost-wire", "fixture", run.prices, cfg, kind),
        ledger,
    )
}
fn history(length: usize) -> Vec<SessionMessage> {
    vec![SessionMessage {
        user: None,
        recorded_diff: None,
        role: MessageRole::User,
        content: "x".repeat(length).into(),
        estimated_tokens: 100,
        tool_call_id: None,
        tool: None,
    }]
}

#[test]
fn session_token_budget_stops_unpriced_runs_through_the_shared_account() {
    let root = crate::tests::OwnedTestDirectory::new("token-budget-run");
    let account = Account::new("native-cost-session", None, 0.0).unwrap();
    account.set_token_caps(None, Some(100));
    let run = Run::fixture(
        account.clone(),
        Arc::new(Ledger::new(root.path())),
        "native-cost-session",
        PriceSnapshot::capture(None, Default::default(), false),
    );
    // A subagent run shares the parent's account, so its usage counts.
    let child = run.with_kind(RequestKind::Subagent);
    assert!(run.admit().is_ok());
    child.observe(crate::event::UsageDelta {
        input_tokens: 70,
        output_tokens: 20,
        total_tokens: 90,
        ..Default::default()
    });
    assert!(run.admit().is_ok());
    run.observe(crate::event::UsageDelta {
        input_tokens: 5,
        output_tokens: 5,
        total_tokens: 10,
        ..Default::default()
    });
    assert!(run.admit().is_err());
    assert!(child.admit().is_err());
    assert!(run.admit_effect().is_err());
    assert!(run.was_refused());
}

#[tokio::test]
async fn usd_budget_native_empty_compaction_records_usage_and_blocks_next_request() {
    let root = crate::tests::OwnedTestDirectory::new("cost-native-empty");
    let wire = Wire::new(vec![Some(response("", false)), None]).await;
    let (client, cfg) = wire.client();
    let (run, ledger) = profile(&root, &cfg, RequestKind::Compaction, Some(0.00001));
    let observed = UsageObserver::with_run(run.clone());
    let result = client
        .compress_messages_observed(
            "fixture",
            &history(100),
            None,
            None,
            12000,
            1000,
            &cfg.retry,
            observed.clone(),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("empty response"));
    assert_eq!(observed.total().input_tokens, 50);
    let refused = client
        .compress_messages_observed(
            "fixture",
            &history(100),
            None,
            None,
            12000,
            1000,
            &cfg.retry,
            observed.clone(),
        )
        .await;
    assert!(
        refused
            .unwrap_err()
            .to_string()
            .contains(super::run::BUDGET_STOP)
    );
    assert_eq!(wire.calls.load(Ordering::Acquire), 1);
    let totals = ledger
        .summary_for_session("native-cost-session")
        .unwrap()
        .session;
    assert_eq!(totals.records, 1);
    assert_eq!(totals.output_tokens, 10);
    assert!((totals.known_cost - 0.00007).abs() < 1e-12);
    let body = wire.bodies.lock().unwrap();
    assert!(body[0]["tools"].as_array().is_none_or(Vec::is_empty));
    assert_eq!(body[0]["max_tokens"], 1000);
    let row: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(root.path().join("usage.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(row["kind"], "compaction");
    assert_eq!(row["provider"], "cost-wire");
    assert_eq!(row["prices"]["input"], 1.0);
}

#[tokio::test]
async fn usd_budget_native_rolling_compaction_cancellation_retains_first_receipt() {
    let root = crate::tests::OwnedTestDirectory::new("cost-native-rolling-cancel");
    let wire = Wire::new(vec![Some(response("A retained summary", false)), None]).await;
    let (client, cfg) = wire.client();
    let (run, ledger) = profile(&root, &cfg, RequestKind::Compaction, None);
    let observed = UsageObserver::with_run(run.clone());
    let messages = history(20000);
    {
        let operation = client.compress_messages_observed(
            "fixture",
            &messages,
            None,
            None,
            4000,
            1000,
            &cfg.retry,
            observed.clone(),
        );
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => panic!("rolling operation completed before cancellation: {result:?}"),
            _ = wire.wait_calls(2) => {}
        }
        // Poll the opened second stream through its text chunk, then cancel the
        // operation while its actual response is awaiting canonical Final.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut operation)
                .await
                .is_err()
        );
    }
    assert!(run.account.snapshot().tracking_error);
    run.account.set_cap(Some(1.0)).unwrap();
    assert!(run.admit().is_err());
    assert_eq!(observed.total().input_tokens, 50);
    assert_eq!(ledger.summary().unwrap().all_time.records, 1);
    assert!((run.account.snapshot().recorded_spend() - 0.00007).abs() < 1e-12);
}

#[cfg(feature = "goal")]
#[tokio::test]
async fn usd_budget_native_judge_hallucinated_tool_keeps_receipt_without_followup_or_effect() {
    let root = crate::tests::OwnedTestDirectory::new("cost-native-judge-tool");
    let wire = Wire::new(vec![Some(response("", true)), None]).await;
    let (client, cfg) = wire.client();
    let (run, ledger) = profile(&root, &cfg, RequestKind::Judge, None);
    let observed = UsageObserver::with_run(run);
    let result = client
        .judge_completion_observed(
            "fixture",
            "judge only".into(),
            "no tools".into(),
            64,
            &cfg.retry,
            observed.clone(),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(wire.calls.load(Ordering::Acquire), 1);
    assert_eq!(observed.total().input_tokens, 50);
    assert_eq!(ledger.summary().unwrap().all_time.records, 1);
    assert!(!root.path().join("NEVER").exists());
    assert_eq!(wire.bodies.lock().unwrap()[0]["max_tokens"], 64);
}

#[tokio::test]
async fn usd_budget_native_primary_and_repair_unknown_tools_fail_closed_on_unread_usage() {
    for repair in [false, true] {
        let root = crate::tests::OwnedTestDirectory::new("cost-native-primary-invalid");
        let wire = Wire::new(vec![Some(response("", true)), None]).await;
        let (client, cfg) = wire.client();
        let (run, ledger) = profile(
            &root,
            &cfg,
            if repair {
                RequestKind::Repair
            } else {
                RequestKind::Turn
            },
            Some(0.0001),
        );
        let agent =
            if repair {
                client
                    .output_schema_repair_agent(
                        "fixture",
                        &cfg,
                        "native-cost-session",
                        64,
                        None,
                        None,
                        None,
                        None,
                    )
                    .unwrap()
            } else {
                let crate::provider::AnyModel::OpenAI(crate::provider::OpenAiModel::Completions(
                    model,
                )) = client.completion_model("fixture")
                else {
                    panic!("fixture protocol")
                };
                crate::provider::AnyAgent::without_skills(crate::provider::AnyAgentInner::OpenAI(
                    crate::provider::OpenAiAgent::Completions(
                        rig::agent::AgentBuilder::new(model).build(),
                    ),
                ))
            }
            .with_spend(Some(run.clone()));
        let turn = agent
            .run_print_with_output(
                "return one answer",
                crate::print::HeadlessOutputPolicy::legacy(false, false),
                &cfg.retry,
                Vec::new(),
                #[cfg(feature = "hooks")]
                None,
            )
            .await;
        assert!(turn.failure.is_some());
        // The SDK refused the tool before consuming its later usage chunk.
        // No fabricated receipt is written, and the incomplete state survives
        // checkpoint serialization and blocks required continuation/effects.
        assert_eq!(turn.usage.input_tokens, 0);
        assert_eq!(ledger.summary().unwrap().all_time.records, 0);
        assert!(run.account.snapshot().tracking_error);
        assert!(run.admit().is_err());
        assert!(run.admit_effect().is_err());
        let saved = serde_json::to_vec(&run.account).unwrap();
        let restored: Account = serde_json::from_slice(&saved).unwrap();
        restored.set_cap(Some(1.0)).unwrap();
        assert!(restored.admit(run.prices).is_err());
        assert_eq!(wire.calls.load(Ordering::Acquire), 1);
        assert!(!root.path().join("NEVER").exists());
    }
}

#[tokio::test]
async fn usd_budget_native_title_captured_profile_charges_once_and_blocks_without_task_local() {
    let root = crate::tests::OwnedTestDirectory::new("cost-native-title");
    let wire = Wire::new(vec![Some(response("Helper Cost", false)), None]).await;
    let (client, cfg) = wire.client();
    let (run, ledger) = profile(&root, &cfg, RequestKind::Title, Some(0.00001));
    let request = crate::session::title::TitleRequest {
        session_id: "native-cost-session".into(),
        provider: "cost-wire".into(),
        model: "fixture".into(),
        prompt: "opening exchange".into(),
    };
    let observed = crate::session::title::UsageLedger::with_spend(run.clone());
    assert_eq!(
        client
            .title_completion(&request, &cfg, observed.clone())
            .await
            .unwrap(),
        "Helper Cost"
    );
    assert_eq!(observed.take().input_tokens, 50);
    assert!(!observed.take().has_values());
    assert!(
        client
            .title_completion(&request, &cfg, observed)
            .await
            .unwrap_err()
            .to_string()
            .contains(super::run::BUDGET_STOP)
    );
    assert_eq!(wire.calls.load(Ordering::Acquire), 1);
    assert_eq!(ledger.summary().unwrap().all_time.records, 1);
    assert!((run.account.snapshot().recorded_spend() - 0.00007).abs() < 1e-12);
    assert_eq!(wire.bodies.lock().unwrap()[0]["max_tokens"], 64);
}

#[cfg(feature = "git-worktree")]
#[tokio::test]
async fn usd_budget_native_git_proposal_uses_captured_receipt_and_blocks_next_request() {
    let root = crate::tests::OwnedTestDirectory::new("cost-native-git-proposal");
    let wire = Wire::new(vec![
        Some(response("{\"message\":\"Captured proposal\"}", false)),
        None,
    ])
    .await;
    let (client, cfg) = wire.client();
    let (run, ledger) = profile(&root, &cfg, RequestKind::GitProposal, Some(0.00001));
    let usage = crate::ui::slash::git::UsageLedger::with_spend(run.clone());
    let answer = client
        .git_proposal_completion(
            "fixture",
            &cfg,
            "native-cost-session",
            "owned prompt".into(),
            "proposal only".into(),
            usage.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&answer).unwrap()["message"],
        "Captured proposal"
    );
    assert_eq!(usage.take().input_tokens, 50);
    assert!(!usage.take().has_values());
    assert!(
        client
            .git_proposal_completion(
                "fixture",
                &cfg,
                "native-cost-session",
                "never again".into(),
                "proposal only".into(),
                usage
            )
            .await
            .unwrap_err()
            .to_string()
            .contains(super::run::BUDGET_STOP)
    );
    assert_eq!(wire.calls.load(Ordering::Acquire), 1);
    assert_eq!(wire.bodies.lock().unwrap()[0]["max_tokens"], 1024);
    assert_eq!(ledger.summary().unwrap().all_time.records, 1);
    let row: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(root.path().join("usage.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(row["kind"], "git_proposal");
    assert_eq!(row["provider"], "cost-wire");
    assert!((run.account.snapshot().recorded_spend() - 0.00007).abs() < 1e-12);
}

fn primary_agent(client: &AnyClient, run: Run) -> crate::provider::AnyAgent {
    let crate::provider::AnyModel::OpenAI(crate::provider::OpenAiModel::Completions(model)) =
        client.completion_model("fixture")
    else {
        panic!("fixture protocol")
    };
    crate::provider::AnyAgent::without_skills(crate::provider::AnyAgentInner::OpenAI(
        crate::provider::OpenAiAgent::Completions(rig::agent::AgentBuilder::new(model).build()),
    ))
    .with_spend(Some(run))
}

#[tokio::test]
async fn usd_budget_native_primary_success_repeated_and_sibling_profiles_charge_once() {
    let root = crate::tests::OwnedTestDirectory::new("cost-primary-settlement");
    let wire = Wire::new(vec![
        Some(response("accepted", false)),
        Some(response("accepted again", false)),
    ])
    .await;
    let sibling_wire = Wire::new(vec![Some(response("sibling", false))]).await;
    let (client, cfg) = wire.client();
    let (sibling_client, _) = sibling_wire.client();
    let (run, ledger) = profile(&root, &cfg, RequestKind::Turn, Some(1.0));
    let sibling = run.with_captured_profile(
        "other-provider",
        "other-model",
        run.prices,
        &cfg,
        RequestKind::Btw,
    );
    let agent = primary_agent(&client, run.clone());
    let other = primary_agent(&sibling_client, sibling.clone());
    let (first, second) = tokio::join!(
        agent.run_print_with_output(
            "first",
            crate::print::HeadlessOutputPolicy::legacy(false, false),
            &cfg.retry,
            Vec::new(),
            #[cfg(feature = "hooks")]
            None
        ),
        other.run_print_with_output(
            "sibling",
            crate::print::HeadlessOutputPolicy::legacy(false, false),
            &cfg.retry,
            Vec::new(),
            #[cfg(feature = "hooks")]
            None
        ),
    );
    assert!(first.failure.is_none() && second.failure.is_none());
    assert_eq!(first.usage.input_tokens, 50);
    let third = agent
        .run_print_with_output(
            "next",
            crate::print::HeadlessOutputPolicy::legacy(false, false),
            &cfg.retry,
            Vec::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(third.failure.is_none());
    assert_eq!(third.usage.input_tokens, 50);
    let totals = ledger.summary().unwrap().all_time;
    assert_eq!(totals.records, 3);
    assert_eq!(totals.input_tokens, 150);
    assert!((totals.known_cost - 0.00021).abs() < 1e-12);
    assert!(!run.account.snapshot().tracking_error);
    assert!(run.admit().is_ok() && sibling.admit_effect().is_ok());
    assert_eq!(wire.calls.load(Ordering::Acquire), 2);
    assert_eq!(sibling_wire.calls.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn usd_budget_native_primary_cancel_before_final_preserves_incomplete_state_without_receipt()
{
    let root = crate::tests::OwnedTestDirectory::new("cost-primary-cancel");
    let wire = Wire::new(vec![None]).await;
    let (client, cfg) = wire.client();
    let (run, ledger) = profile(&root, &cfg, RequestKind::Turn, Some(1.0));
    let agent = primary_agent(&client, run.clone());
    {
        let work = agent.run_print_with_output(
            "wait",
            crate::print::HeadlessOutputPolicy::legacy(false, false),
            &cfg.retry,
            Vec::new(),
            #[cfg(feature = "hooks")]
            None,
        );
        tokio::pin!(work);
        tokio::select! {
            result=&mut work=>panic!("unexpected completion: {:?}",result.failure),
            _=tokio::time::sleep(std::time::Duration::from_millis(150))=>{}
        }
        assert_eq!(wire.calls.load(Ordering::Acquire), 1);
        assert!(
            !run.account.snapshot().tracking_error,
            "active stream is not retired yet"
        );
    }
    assert!(run.account.snapshot().tracking_error);
    assert!(run.admit().is_err() && run.admit_effect().is_err());
    assert_eq!(ledger.summary().unwrap().all_time.records, 0);
    let restored: Account =
        serde_json::from_slice(&serde_json::to_vec(&run.account).unwrap()).unwrap();
    restored.set_cap(Some(1.0)).unwrap();
    assert!(restored.admit(run.prices).is_err());
}

#[tokio::test]
async fn usd_budget_native_primary_cancel_after_canonical_final_keeps_receipt_without_projection() {
    use futures::StreamExt;
    use rig::streaming::StreamingChat;
    #[derive(Clone)]
    struct HoldAccepted(Arc<tokio::sync::Notify>);
    impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for HoldAccepted {
        async fn on_event(
            &self,
            _: &rig::agent::HookContext,
            event: rig::agent::StepEvent<'_, M>,
        ) -> rig::agent::Flow {
            if matches!(event, rig::agent::StepEvent::ModelTurnFinished { .. }) {
                self.0.notify_one();
                std::future::pending::<()>().await;
            }
            rig::agent::Flow::Continue
        }
    }
    let root = crate::tests::OwnedTestDirectory::new("cost-primary-after-final");
    let wire = Wire::new(vec![Some(response("accepted", false))]).await;
    let (client, cfg) = wire.client();
    let (run, ledger) = profile(&root, &cfg, RequestKind::Turn, Some(1.0));
    let crate::provider::AnyModel::OpenAI(crate::provider::OpenAiModel::Completions(model)) =
        client.completion_model("fixture")
    else {
        panic!("fixture protocol")
    };
    let agent = rig::agent::AgentBuilder::new(model).build();
    let accepted = Arc::new(tokio::sync::Notify::new());
    {
        let work = super::run::scope(Some(run.clone()), async {
            let mut stream = agent
                .stream_chat(
                    "accepted before output",
                    Vec::<rig::completion::Message>::new(),
                )
                .add_hook(super::run::AdmissionHook(Some(run.clone())))
                .add_hook(HoldAccepted(accepted.clone()))
                .await;
            while let Some(item) = stream.next().await {
                item.unwrap();
            }
        });
        tokio::pin!(work);
        tokio::select! {
            _=&mut work=>panic!("accepted hook must hold"),
            _=tokio::time::timeout(std::time::Duration::from_secs(3),accepted.notified())=>{}
        }
        assert_eq!(ledger.summary().unwrap().all_time.records, 1);
    }
    assert!(!run.account.snapshot().tracking_error);
    assert_eq!(ledger.summary().unwrap().all_time.input_tokens, 50);
    assert!((run.account.snapshot().recorded_spend() - 0.00007).abs() < 1e-12);
    assert!(run.admit().is_ok());
}

#[tokio::test]
async fn usd_budget_native_primary_before_success_transport_error_preserves_retry_and_receipts() {
    let root = crate::tests::OwnedTestDirectory::new("cost-primary-retry");
    let wire=Wire::new(vec![Some("HTTP/1.1 429 Too Many Requests\r\nContent-Length: 2\r\nConnection: close\r\nRetry-After: 0\r\n\r\n{}".into()),Some(response("retried",false))]).await;
    let (client, mut cfg) = wire.client();
    cfg.retry.max_attempts = 2;
    cfg.retry.initial_backoff_ms = 1;
    let (run, ledger) = profile(&root, &cfg, RequestKind::Turn, Some(1.0));
    let answer = primary_agent(&client, run.clone())
        .run_print_with_output(
            "retry",
            crate::print::HeadlessOutputPolicy::legacy(false, false),
            &cfg.retry,
            Vec::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(answer.failure.is_none(), "{:?}", answer.failure);
    assert_eq!(wire.calls.load(Ordering::Acquire), 2);
    assert_eq!(ledger.summary().unwrap().all_time.records, 1);
    assert_eq!(answer.usage.input_tokens, 50);
    assert!(!run.account.snapshot().tracking_error);
    assert!(run.admit().is_ok());
}

#[tokio::test]
async fn usd_budget_native_primary_tool_continuation_never_reuses_previous_request_credit() {
    use rig::tool::Tool;
    struct CountWrite(Arc<AtomicUsize>);
    impl Tool for CountWrite {
        const NAME: &'static str = "write";
        type Args = serde_json::Value;
        type Output = String;
        type Error = crate::agent::tools::ToolError;
        fn description(&self) -> String {
            "Count the actual accepted fixture effect".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn call(&self, _: Self::Args) -> Result<String, Self::Error> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok("accepted effect".into())
        }
    }
    let root = crate::tests::OwnedTestDirectory::new("cost-primary-tool-continuation");
    let wire = Wire::new(vec![
        Some(response("", true)),
        Some(response("completed", false)),
    ])
    .await;
    let (client, cfg) = wire.client();
    let (run, ledger) = profile(&root, &cfg, RequestKind::Turn, Some(1.0));
    let effects = Arc::new(AtomicUsize::new(0));
    let crate::provider::AnyModel::OpenAI(crate::provider::OpenAiModel::Completions(model)) =
        client.completion_model("fixture")
    else {
        panic!("fixture protocol")
    };
    let agent = crate::provider::AnyAgent::without_skills(crate::provider::AnyAgentInner::OpenAI(
        crate::provider::OpenAiAgent::Completions(
            rig::agent::AgentBuilder::new(model)
                .tool(CountWrite(effects.clone()))
                .default_max_turns(2)
                .build(),
        ),
    ))
    .with_spend(Some(run.clone()));
    let answer = agent
        .run_print_with_output(
            "use fixture then finish",
            crate::print::HeadlessOutputPolicy::legacy(false, false),
            &cfg.retry,
            Vec::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(answer.failure.is_none(), "{:?}", answer.failure);
    assert_eq!(effects.load(Ordering::Acquire), 1);
    assert_eq!(wire.calls.load(Ordering::Acquire), 2);
    assert_eq!(answer.usage.input_tokens, 100);
    let totals = ledger.summary().unwrap().all_time;
    assert_eq!(totals.records, 2);
    assert_eq!(totals.input_tokens, 100);
    assert!((totals.known_cost - 0.00014).abs() < 1e-12);
    assert!(!run.account.snapshot().tracking_error);
    assert!(
        answer
            .interactions
            .iter()
            .any(|m| matches!(m, rig::completion::Message::User { .. }))
    );
}

#[cfg(feature = "advisor")]
#[tokio::test]
async fn usd_budget_native_advisor_receipt_is_retained_and_next_request_refused() {
    let root = crate::tests::OwnedTestDirectory::new("cost-native-advisor");
    let wire = Wire::new(vec![Some(response("captured guidance", false)), None]).await;
    let (client, cfg) = wire.client();
    let (run, ledger) = profile(&root, &cfg, RequestKind::Advisor, Some(0.00001));
    let model = client.completion_model("fixture");
    let first = super::run::scope(
        Some(run.clone()),
        crate::extras::advisor::run_advisor_completion(model, "question", &[], 1, "fixture", &cfg),
    )
    .await
    .unwrap();
    assert_eq!(first, "captured guidance");
    let denied = super::run::scope(
        Some(run.clone()),
        crate::extras::advisor::run_advisor_completion(
            client.completion_model("fixture"),
            "never again",
            &[],
            1,
            "fixture",
            &cfg,
        ),
    )
    .await;
    assert!(denied.is_err());
    assert_eq!(wire.calls.load(Ordering::Acquire), 1);
    let totals = ledger.summary().unwrap().all_time;
    assert_eq!(totals.records, 1);
    assert_eq!(totals.input_tokens, 50);
    assert!((totals.known_cost - 0.00007).abs() < 1e-12);
    let row: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(root.path().join("usage.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(row["kind"], "advisor");
}

#[tokio::test]
async fn usd_budget_native_primary_missing_usage_blocks_paid_but_preserves_known_free_rates() {
    for case in 0..5 {
        let free = case == 1;
        let root = crate::tests::OwnedTestDirectory::new("cost-primary-missing-usage");
        let with_usage = response("nonempty answer with unreported metrics", false);
        let missing = with_usage
            .split("\n\n")
            .map(|part| {
                if let Some(json) = part.strip_prefix("data: ")
                    && let Ok(mut json) = serde_json::from_str::<serde_json::Value>(json)
                {
                    json.as_object_mut().unwrap().remove("usage");
                    return format!("data: {json}");
                }
                part.to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let wire = Wire::new(vec![Some(missing)]).await;
        let (client, cfg) = wire.client();
        let (mut run, ledger) = profile(&root, &cfg, RequestKind::Turn, Some(1.0));
        if case >= 1 {
            // The same canonical SDK missing-usage path must honor the
            // captured native cache tiers independently of free base rates.
            let cache = crate::pricing::CachePrices {
                read: Some(if case == 2 { 0.5 } else { 0.0 }),
                write: Some(if case == 3 { 0.5 } else { 0.0 }),
                write_1h: Some(if case == 4 { 0.5 } else { 0.0 }),
                split_required: false,
            };
            run = run.with_captured_profile(
                "cost-wire",
                "fixture",
                PriceSnapshot::capture(Some((0.0, 0.0)), cache, case >= 2),
                &cfg,
                RequestKind::Turn,
            );
        }
        let turn = primary_agent(&client, run.clone())
            .run_print_with_output(
                "one answer",
                crate::print::HeadlessOutputPolicy::legacy(false, false),
                &cfg.retry,
                Vec::new(),
                #[cfg(feature = "hooks")]
                None,
            )
            .await;
        assert!(turn.failure.is_none(), "{:?}", turn.failure);
        assert!(!turn.response.is_empty());
        assert_eq!(turn.usage.input_tokens, 0);
        assert_eq!(wire.calls.load(Ordering::Acquire), 1);
        let totals = ledger.summary().unwrap().all_time;
        assert_eq!(totals.records, 1);
        assert_eq!(totals.unreported_records, 1);
        assert_eq!(totals.unpriced_records, usize::from(!free));
        assert_eq!(run.account.snapshot().tracking_error, !free);
        assert_eq!(run.admit().is_ok(), free);
        assert_eq!(run.admit_effect().is_ok(), free);
        let row: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(root.path().join("usage.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(row["usage_reported"], false);
        assert_eq!(
            row["cost"],
            if free {
                serde_json::json!(0.0)
            } else {
                serde_json::Value::Null
            }
        );
    }
}

#[cfg(feature = "subagents")]
#[tokio::test]
async fn usd_budget_native_actual_task_batch_held_children_keep_independent_profiles_and_persona_prices()
 {
    use rig::tool::Tool;
    let root = crate::tests::OwnedTestDirectory::new("cost-actual-task-batch");
    let bindings = [
        "ZS_CONFIG_DIR",
        "ZS_DATA_DIR",
        "ZS_LOCAL_DATA_DIR",
        "ZS_STATE_DIR",
        "ZS_CACHE_DIR",
        "ZS_CREDENTIALS_DIR",
    ]
    .map(|name| (name, Some(root.path().join(name).into_os_string())));
    let _env = crate::tests::ScopedProcessEnv::set(&bindings);
    std::fs::write(root.path().join("read-a.txt"), "OWNED_CHILD_READ_A").unwrap();
    std::fs::write(root.path().join("read-b.txt"), "OWNED_CHILD_READ_B").unwrap();
    let paths = crate::paths::process_paths_ref().unwrap();
    std::fs::create_dir_all(paths.agents_dir()).unwrap();
    std::fs::write(
        paths.agents_dir().join("cost-fixture.md"),
        "---\nmodel: cost-child\ntools: read\n---\nInvestigate only the delegated fixture data.\n",
    )
    .unwrap();
    // Modify the actual JSON tool delta rather than textual escaped matching.
    let read_response = |path: &str| {
        let raw = response("", true);
        raw.split("\n\n")
            .map(|part| {
                if let Some(json) = part.strip_prefix("data: ")
                    && let Ok(mut json) = serde_json::from_str::<serde_json::Value>(json)
                    && let Some(tool) = json.pointer_mut("/choices/0/delta/tool_calls/0")
                {
                    tool["function"]["name"] = serde_json::json!("read");
                    tool["function"]["arguments"] = serde_json::json!(
                        serde_json::json!({"path":path,"offset":0,"limit":10}).to_string()
                    );
                    return format!("data: {json}");
                }
                part.to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let report = "## Findings\n- [confidence: high] The owned fixture was read.\n\n## Unverified\n- None\n\n## Coverage\n- Covered: delegated fixture.\n- Skipped: None.\n";
    let wire = Wire::new(vec![
        Some(format!("HOLD_PAIR\n{}", read_response("read-a.txt"))),
        Some(format!("HOLD_PAIR\n{}", read_response("read-b.txt"))),
        Some(response(report, false)),
        Some(response(report, false)),
    ])
    .await;
    let (client, cfg) = wire.client();
    let mut value = serde_json::to_value(cfg).unwrap();
    value["quick_models"] = serde_json::json!({"cost-child":{"provider":"cost-wire","model":"fixture","input_token_cost":1.0,"output_token_cost":2.0}});
    value["task_max_concurrency"] = serde_json::json!(2);
    value["task_max_turns"] = serde_json::json!(2);
    value["max_budget_usd"] = serde_json::json!(1.0);
    let cfg: Config = serde_json::from_value(value).unwrap();
    let (run, ledger) = profile(&root, &cfg, RequestKind::Turn, Some(1.0));
    let run = run.with_captured_profile(
        "parent-provider",
        "parent-model",
        PriceSnapshot::capture(Some((10.0, 20.0)), Default::default(), false),
        &cfg,
        RequestKind::Turn,
    );
    run.prepare().await.unwrap();
    let config = crate::extras::subagents::SubagentConfig {
        client,
        provider_name: "cost-wire".into(),
        session_provider: "cost-wire".into(),
        model_name: "fallback-model".into(),
        fallback_notice: None,
        api_key: Some("unused-fixture-key".into()),
        max_turns: 2,
        config: cfg.into(),
    };
    let tool = crate::extras::subagents::task_tool::TaskTool::new(None, None, false)
        .with_workspace_binding(Arc::new(
            crate::paths::WorkspaceBinding::capture(root.path()).unwrap(),
        ));
    let args = crate::extras::subagents::task_tool::TaskArgs {
        prompts: vec![
            "Read read-a.txt and report its contents".into(),
            "Read read-b.txt and report its contents".into(),
        ],
        briefs: None,
        agent_type: Some("cost-fixture".into()),
        run_in_background: false,
    };
    let work = super::run::scope(
        Some(run.clone()),
        crate::extras::subagents::scope_test_config(config, tool.call(args)),
    );
    tokio::pin!(work);
    tokio::select! {
        result=&mut work=>panic!("both held child requests must be admitted: {result:?}"),
        _=wire.wait_calls(2)=>{}
    }
    assert_eq!(wire.calls.load(Ordering::Acquire), 2);
    assert_eq!(run.account.snapshot().recorded_spend(), 0.0);
    assert!(!run.account.snapshot().tracking_error);
    wire.release.notify_one();
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), &mut work)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !output.contains("[failed:") && !output.contains("[partial:"),
        "{output}"
    );
    assert_eq!(wire.calls.load(Ordering::Acquire), 4);
    let bodies = wire.bodies.lock().unwrap();
    for body in bodies.iter() {
        assert_eq!(
            body["model"], "fixture",
            "persona model override must reach actual wire"
        );
        let names = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["read"]);
    }
    let continuations = bodies[2..]
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        continuations.contains("OWNED_CHILD_READ_A")
            && continuations.contains("OWNED_CHILD_READ_B"),
        "{continuations}"
    );
    let totals = ledger.summary().unwrap().all_time;
    assert_eq!(totals.records, 4);
    assert_eq!(totals.input_tokens, 200);
    assert_eq!(totals.output_tokens, 40);
    assert!((totals.known_cost - 0.00028).abs() < 1e-12);
    assert!(!run.account.snapshot().tracking_error);
    for row in std::fs::read_to_string(root.path().join("usage.jsonl"))
        .unwrap()
        .lines()
    {
        let row: serde_json::Value = serde_json::from_str(row).unwrap();
        assert_eq!(row["provider"], "cost-wire");
        assert_eq!(row["model"], "fixture");
        assert_eq!(row["prices"]["input"], 1.0);
        assert_eq!(row["kind"], "subagent");
    }
    assert!(run.admit().is_ok());
}

#[tokio::test]
async fn usd_budget_actual_saved_session_fork_preserves_uncertainty_without_importing_receipts() {
    for case in ["healthy", "unknown", "unread"] {
        let incomplete = case != "healthy";
        let root = crate::tests::OwnedTestDirectory::new("cost-session-fork");
        let (ledger, fork, run, original_id) = {
            let bindings = [
                "ZS_CONFIG_DIR",
                "ZS_DATA_DIR",
                "ZS_LOCAL_DATA_DIR",
                "ZS_STATE_DIR",
                "ZS_CACHE_DIR",
                "ZS_CREDENTIALS_DIR",
            ]
            .map(|name| (name, Some(root.path().join(name).into_os_string())));
            let _env = crate::tests::ScopedProcessEnv::set(&bindings);
            let cfg = Config {
                max_budget_usd: Some(1.0),
                ..Default::default()
            };
            let mut source = crate::session::Session::new(
                "openai",
                "fixture",
                128000,
                root.path().to_str().unwrap(),
            );
            source.set_input_output_prices(Some((1.0, 2.0)));
            source.initialize_spend_account(None).unwrap();
            let original_id = source.id.clone();
            let ledger = Ledger::new(&crate::paths::process_paths_ref().unwrap().state_dir);
            if case == "unread" {
                source.spend_account.tracking_failed();
            } else {
                source
                    .spend_account
                    .observe(&super::Receipt::new(
                        &source.id,
                        "openai",
                        "old-model",
                        RequestKind::Turn,
                        crate::event::UsageDelta {
                            input_tokens: 50,
                            output_tokens: 10,
                            total_tokens: 60,
                            ..Default::default()
                        },
                        PriceSnapshot::capture(
                            if incomplete { None } else { Some((1.0, 2.0)) },
                            Default::default(),
                            false,
                        ),
                    ))
                    .unwrap();
            }
            ledger.flush(&source.spend_account).unwrap();
            let mut fork = crate::session::lock::fork_session(&source).unwrap();
            assert_ne!(fork.id, original_id);
            let run = Run::for_session(&mut fork, &cfg, RequestKind::Turn);
            (ledger, fork, run, original_id)
        };
        assert_eq!(run.prepare().await.is_ok(), !incomplete);
        assert_eq!(run.account.snapshot().tracking_error, incomplete);
        assert_eq!(
            run.account.snapshot().unpriced_tokens,
            if case == "unknown" { 60 } else { 0 }
        );
        assert_eq!(run.admit().is_ok(), !incomplete);
        assert_eq!(run.admit_effect().is_ok(), !incomplete);
        assert_eq!(
            ledger
                .summary_for_session(&fork.id)
                .unwrap()
                .session
                .records,
            0
        );
        assert_eq!(
            ledger
                .summary_for_session(&original_id)
                .unwrap()
                .session
                .records,
            usize::from(case != "unread")
        );
        assert_eq!(
            ledger.summary().unwrap().all_time.records,
            usize::from(case != "unread")
        );
        assert_eq!(run.account.snapshot().observed_cost, 0.0);
        let estimate = if incomplete { 0.0 } else { 0.00007 };
        assert!((run.account.snapshot().legacy_estimate - estimate).abs() < 1e-12);
    }
}

// This child uses the real CLI/startup/HTTP/SDK/interrupt path. Only the
// regular ledger's pre-sync filesystem call is held, in its blocking worker.
#[cfg(unix)]
fn held_ledger_flush_child(root: &std::path::Path) -> ! {
    use clap::Parser;
    let bindings = [
        "ZS_CONFIG_DIR",
        "ZS_DATA_DIR",
        "ZS_LOCAL_DATA_DIR",
        "ZS_STATE_DIR",
        "ZS_CACHE_DIR",
        "ZS_CREDENTIALS_DIR",
    ]
    .map(|key| (key, Some(root.join(key).into_os_string())));
    let _env = crate::tests::ScopedProcessEnv::set(&bindings);
    std::env::set_current_dir(root).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(Box::pin(async {
        let wire = Wire::new(vec![Some(response("retained before filesystem settlement", false))]).await;
        let (_, cfg) = wire.client();
        let mut cfg = serde_json::to_value(cfg).unwrap();
        cfg["quick_models"] = serde_json::json!({"cost-fixture":{"provider":"cost-wire","model":"fixture","input_token_cost":1.0,"output_token_cost":2.0}});
        cfg["model"] = serde_json::json!("cost-fixture");
        cfg["session_title_model"] = serde_json::json!("off");
        cfg["no_tools"] = serde_json::json!(true);
        let paths = crate::paths::process_paths_ref().unwrap();
        crate::fs::ensure_private_directory(&paths.config_dir).unwrap();
        let cfg: Config = serde_json::from_value(cfg).unwrap();
        crate::fs::atomic_create_sync(&paths.config_file(), toml::to_string(&cfg).unwrap().as_bytes()).unwrap();
        let cli = crate::cli::Cli::parse_from([
            "mini-agent", "-p", "retain the observed answer", "--provider", "cost-wire",
            "--api-key", "unused-fixture-key",
            "--output", "json", "--max-budget-usd", "1.0", "--no-context-files",
        ]);
        let result = super::ledger::PreSyncGate::new(root).scope(crate::run(cli)).await;
        std::fs::write(root.join("result"), format!("forced={} result={result:?}", crate::print::headless_force_stopped())).unwrap();
        result
    }));
    if crate::print::headless_force_stopped() {
        crate::print::exit_force_stopped();
    }
    let code = if result.is_ok() { 0 } else { 1 };
    // The release controls must join their actual writer before runtime drop.
    drop(runtime);
    std::process::exit(code);
}

#[cfg(unix)]
#[test]
fn usd_budget_actual_held_regular_ledger_flush_preserves_sigint_grace_and_writer_ownership() {
    const CHILD_ENV: &str = "MINI_AGENT_TEST_COST_LEDGER_IO_GATE";
    if let Some(root) = std::env::var_os(CHILD_ENV) {
        held_ledger_flush_child(std::path::Path::new(&root));
    }
    use std::time::{Duration, Instant};
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    for mode in ["release", "interrupt-release", "second-signal", "grace"] {
        let root = crate::tests::OwnedTestDirectory::new("cost-held-regular-ledger");
        let stdout = std::fs::File::create(root.path().join("stdout")).unwrap();
        let stderr = std::fs::File::create(root.path().join("stderr")).unwrap();
        let mut child = Child(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "spend::request_tests::usd_budget_actual_held_regular_ledger_flush_preserves_sigint_grace_and_writer_ownership", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, root.path()).stdout(stdout).stderr(stderr).spawn().unwrap());
        let ready = Instant::now();
        while !root.path().join("started").exists() {
            assert!(
                ready.elapsed() < Duration::from_secs(30),
                "{mode}: no pre-sync marker; {}",
                std::fs::read_to_string(root.path().join("stderr")).unwrap()
            );
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "{mode}: child exited before real ledger flush: stdout={} stderr={} result={}",
                std::fs::read_to_string(root.path().join("stdout")).unwrap(),
                std::fs::read_to_string(root.path().join("stderr")).unwrap(),
                std::fs::read_to_string(root.path().join("result")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let observed: super::AccountSnapshot =
            serde_json::from_slice(&std::fs::read(root.path().join("started")).unwrap()).unwrap();
        assert!((observed.recorded_spend() - 0.00007).abs() < 1e-12);
        assert!(!observed.tracking_error);
        let stopped = Instant::now();
        if mode != "release" {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child.0.id() as i32),
                nix::sys::signal::Signal::SIGINT,
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(150));
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "first interrupt still owns the stalled writer"
            );
        }
        if mode == "release" || mode == "interrupt-release" {
            std::fs::write(root.path().join("release"), "release exact scoped writer").unwrap();
        }
        if mode == "second-signal" {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child.0.id() as i32),
                nix::sys::signal::Signal::SIGINT,
            )
            .unwrap();
        }
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                stopped.elapsed()
                    < crate::print::HEADLESS_FORCED_STOP_GRACE + Duration::from_secs(5),
                "{mode}: cancellation could not repoll"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let forced = matches!(mode, "second-signal" | "grace");
        assert_eq!(
            status.code(),
            Some(if forced {
                130
            } else if mode == "release" {
                0
            } else {
                1
            }),
            "{mode}: {}",
            std::fs::read_to_string(root.path().join("stderr")).unwrap()
        );
        assert_eq!(root.path().join("completed").exists(), !forced);
        if mode == "second-signal" {
            assert!(stopped.elapsed() < crate::print::HEADLESS_FORCED_STOP_GRACE);
        }
        let output = std::fs::read_to_string(root.path().join("stdout")).unwrap();
        let result: serde_json::Value = output
            .lines()
            .find_map(|line| {
                serde_json::from_str::<serde_json::Value>(
                    &line[line.find('{').unwrap_or(line.len())..],
                )
                .ok()
                .filter(|value| value.get("usage").is_some() && value.get("result").is_some())
            })
            .unwrap_or_else(|| panic!("{mode}: missing production result: {output}"));
        assert_eq!(result["usage"]["input_tokens"], 50, "{result}");
        assert_eq!(result["usage"]["output_tokens"], 10, "{result}");
        let ledger = Ledger::new(&root.path().join("ZS_STATE_DIR"));
        let total = ledger.summary().unwrap().all_time;
        assert_eq!(total.records, 1, "{mode}: exact receipt persisted once");
        assert!((total.known_cost - 0.00007).abs() < 1e-12);
        let sessions = std::fs::read_dir(root.path().join("ZS_STATE_DIR/sessions")).unwrap();
        let saved = sessions
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
            })
            .unwrap();
        let saved: crate::session::Session =
            serde_json::from_slice(&std::fs::read(saved.path()).unwrap()).unwrap();
        assert_eq!(saved.total_input_tokens, 50);
        assert_eq!(saved.total_output_tokens, 10);
        assert!((saved.recorded_total_cost() - 0.00007).abs() < 1e-12);
        assert_eq!(saved.spend_account.snapshot().tracking_error, forced);
        assert!(
            saved.messages.iter().any(|message| message
                .content
                .contains("retained before filesystem settlement")),
            "{mode}: accepted answer history is retained"
        );
    }
}

#[tokio::test]
async fn usd_budget_native_reader_contention_releases_for_canonical_receipt() {
    let root = crate::tests::OwnedTestDirectory::new("cost-reader-contention");
    let path = root.path().join("usage.jsonl");
    crate::fs::private_atomic_create_sync(&path, b"").unwrap();
    let reader =
        crate::fs::open_private_file_checked(&path, crate::fs::PrivateFileAccess::Read).unwrap();
    reader.try_lock_shared().unwrap();
    let (blocked, seen) = std::sync::mpsc::sync_channel(1);
    let ledger = Arc::new(Ledger::new(root.path()).with_lock_signal(blocked));
    let account = Account::new("native-cost-session", Some(1.0), 0.0).unwrap();
    let wire = Wire::new(vec![Some(response("accepted with a reader", false))]).await;
    let (client, cfg) = wire.client();
    let run = Run::fixture(
        account,
        ledger.clone(),
        "native-cost-session",
        PriceSnapshot::capture(
            Some((1.0, 2.0)),
            crate::pricing::CachePrices::default(),
            false,
        ),
    )
    .with_captured_profile(
        "cost-wire",
        "fixture",
        PriceSnapshot::capture(
            Some((1.0, 2.0)),
            crate::pricing::CachePrices::default(),
            false,
        ),
        &cfg,
        RequestKind::Turn,
    );
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let outcome = scope
        .run(async {
            let release = crate::agent::runner::spawn_blocking_scoped(move || {
                let observed = seen.recv_timeout(std::time::Duration::from_secs(3));
                drop(reader);
                observed.expect("real writer lock contention before releasing reader");
            });
            let agent = primary_agent(&client, run.clone());
            let (outcome, released) = tokio::join!(
                agent.run_print_with_output(
                    "first",
                    crate::print::HeadlessOutputPolicy::legacy(false, false),
                    &cfg.retry,
                    Vec::new(),
                    #[cfg(feature = "hooks")]
                    None,
                ),
                release
            );
            released.unwrap();
            outcome
        })
        .await;
    cleanup.settle().await;
    assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
    assert_eq!(wire.calls.load(Ordering::Acquire), 1);
    assert_eq!(outcome.usage.input_tokens, 50);
    assert!(
        !run.account.snapshot().tracking_error,
        "temporary reader contention must not poison tracking"
    );
    assert!(!run.account.has_pending());
    let totals = ledger.summary().unwrap().all_time;
    assert_eq!(
        (totals.records, totals.input_tokens, totals.output_tokens),
        (1, 50, 10)
    );
    assert!((totals.known_cost - 0.00007).abs() < 1e-12);
}

#[tokio::test]
async fn usd_budget_native_reader_contention_report_is_readonly_for_paid_and_free_pending_receipts()
{
    for free in [false, true] {
        let root = crate::tests::OwnedTestDirectory::new("cost-reader-report");
        let path = root.path().join("usage.jsonl");
        crate::fs::private_atomic_create_sync(&path, b"").unwrap();
        let reader =
            crate::fs::open_private_file_checked(&path, crate::fs::PrivateFileAccess::Read)
                .unwrap();
        reader.try_lock_shared().unwrap();
        let (blocked, seen) = std::sync::mpsc::sync_channel(1);
        let ledger = Arc::new(Ledger::new(root.path()).with_lock_signal(blocked));
        let account = Account::new("native-cost-session", Some(1.0), 0.0).unwrap();
        let wire = Wire::new(vec![Some(response("pending but observed", false))]).await;
        let (client, cfg) = wire.client();
        let prices = PriceSnapshot::capture(
            Some(if free { (0.0, 0.0) } else { (1.0, 2.0) }),
            Default::default(),
            false,
        );
        let run = Run::fixture(account, ledger.clone(), "native-cost-session", prices)
            .with_captured_profile("cost-wire", "fixture", prices, &cfg, RequestKind::Turn);
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
        scope
            .run(async {
                let mut notified = crate::agent::runner::spawn_blocking_scoped(move || {
                    seen.recv_timeout(std::time::Duration::from_secs(3))
                });
                let agent = primary_agent(&client, run.clone());
                let mut future = Box::pin(agent.run_print_with_output(
                    "first",
                    crate::print::HeadlessOutputPolicy::legacy(false, false),
                    &cfg.retry,
                    Vec::new(),
                    #[cfg(feature = "hooks")]
                    None,
                ));
                tokio::select! {
                    result = &mut notified => { result.unwrap().unwrap(); },
                    _ = &mut future => panic!("writer must wait while reader is retained"),
                }
                assert!(run.account.has_pending());
                assert!(!run.account.snapshot().tracking_error);
                let mut session = crate::session::Session::new(
                    "cost-wire",
                    "fixture",
                    128_000,
                    root.path().to_str().unwrap(),
                );
                session.id = "native-cost-session".into();
                session.spend_account = run.account.clone();
                let lines = super::cost_lines(&session, Ledger::new(root.path()))
                    .await
                    .unwrap();
                assert!(
                    lines
                        .iter()
                        .any(|line| line.contains("awaiting persistence"))
                );
                assert!(
                    lines
                        .iter()
                        .any(|line| line.starts_with("Session ledger: $0.000000; input 0"))
                );
                assert!(lines.iter().any(|line| line.starts_with(if free {
                    "Session recorded spend: $0.000000"
                } else {
                    "Session recorded spend: $0.000070"
                })));
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    b"",
                    "query must not flush, repair or create a durable receipt"
                );
                assert!(run.account.has_pending());
                drop(reader);
                let result = future.await;
                assert!(result.failure.is_none());
            })
            .await;
        cleanup.settle().await;
        assert_eq!(ledger.summary().unwrap().all_time.records, 1);
        assert!(!run.account.has_pending());
        assert!(!run.account.snapshot().tracking_error);
        assert_eq!(wire.calls.load(Ordering::Acquire), 1);
    }
}

#[tokio::test]
async fn usd_budget_native_reader_contention_deadline_and_cancellation_join_writer_without_losing_charge()
 {
    for cancel in [false, true] {
        let root = crate::tests::OwnedTestDirectory::new("cost-reader-deadline");
        let path = root.path().join("usage.jsonl");
        crate::fs::private_atomic_create_sync(&path, b"").unwrap();
        let reader =
            crate::fs::open_private_file_checked(&path, crate::fs::PrivateFileAccess::Read)
                .unwrap();
        reader.try_lock_shared().unwrap();
        let (blocked, seen) = std::sync::mpsc::sync_channel(1);
        let ledger = Arc::new(Ledger::new(root.path()).with_lock_signal(blocked));
        let wire = Wire::new(vec![Some(response("retained despite reader", false))]).await;
        let (client, cfg) = wire.client();
        let prices = PriceSnapshot::capture(Some((1.0, 2.0)), Default::default(), false);
        let run = Run::fixture(
            Account::new("native-cost-session", Some(1.0), 0.0).unwrap(),
            ledger.clone(),
            "native-cost-session",
            prices,
        )
        .with_captured_profile("cost-wire", "fixture", prices, &cfg, RequestKind::Turn);
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
        let started = std::time::Instant::now();
        scope
            .run(async {
                let mut notified = crate::agent::runner::spawn_blocking_scoped(move || {
                    seen.recv_timeout(std::time::Duration::from_secs(3))
                });
                use futures::StreamExt;
                use rig::streaming::StreamingChat;
                let crate::provider::AnyModel::OpenAI(crate::provider::OpenAiModel::Completions(
                    model,
                )) = client.completion_model("fixture")
                else {
                    panic!("fixture protocol")
                };
                let agent = rig::agent::AgentBuilder::new(model).build();
                // Keep the canonical hook's blocking child in this explicit
                // work scope across dropping its SDK await.
                let mut future = Box::pin(super::run::scope(Some(run.clone()), async {
                    let mut stream = agent
                        .stream_chat("first", Vec::<rig::completion::Message>::new())
                        .add_hook(super::run::AdmissionHook(Some(run.clone())))
                        .tool_extensions(super::run::current_extensions())
                        .await;
                    while let Some(_result) = stream.next().await {}
                }));
                tokio::select! {
                    result = &mut notified => { result.unwrap().unwrap(); },
                    _ = &mut future => panic!("writer must wait while reader is retained"),
                }
                assert!(run.account.has_pending());
                assert!(
                    !run.account.snapshot().tracking_error,
                    "ordinary waiting is not a permanent error"
                );
                assert_eq!(run.accepted_usage().input_tokens, 50);
                if !cancel {
                    future.await;
                }
                // In the cancellation branch dropping this owned await must leave
                // the blocking writer registered for cleanup, not detach it.
            })
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(3), cleanup.settle())
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        assert!(
            run.account.snapshot().tracking_error,
            "deadline is a durable incomplete checkpoint"
        );
        assert!((run.account.snapshot().recorded_spend() - 0.00007).abs() < 1e-12);
        assert!(run.account.has_pending());
        assert!(run.admit().is_err() && run.admit_effect().is_err());
        assert_eq!(run.accepted_usage().input_tokens, 50);
        assert_eq!(wire.calls.load(Ordering::Acquire), 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        let saved = serde_json::to_value(&run.account).unwrap();
        assert_eq!(saved["tracking_error"], true);
        drop(reader);
        // Reattempt can persist the same charged receipt once but must not
        // clear a saved uncertainty flag or admit more capped work.
        ledger.flush(&run.account).unwrap();
        ledger.flush(&run.account).unwrap();
        assert_eq!(ledger.summary().unwrap().all_time.records, 1);
        assert!(!run.account.has_pending());
        assert!((run.account.snapshot().recorded_spend() - 0.00007).abs() < 1e-12);
        assert!(run.admit().is_err());
    }
}

#[tokio::test]
async fn usd_budget_readonly_report_rechecks_paid_and_free_observation_after_earlier_scan() {
    for (free, pending_at_start) in [(false, false), (true, false), (false, true), (true, true)] {
        let root = crate::tests::OwnedTestDirectory::new("cost-report-observation-race");
        let path = root.path().join("usage.jsonl");
        crate::fs::private_atomic_create_sync(&path, b"").unwrap();
        let mut session = crate::session::Session::new(
            "cost-wire",
            "fixture",
            128_000,
            root.path().to_str().unwrap(),
        );
        session.spend_account = Account::new(&session.id, Some(1.0), 0.0).unwrap();
        let receipt = super::Receipt::new(
            &session.id,
            "cost-wire",
            "fixture",
            RequestKind::Turn,
            crate::event::UsageDelta {
                input_tokens: 50,
                output_tokens: 10,
                total_tokens: 60,
                ..Default::default()
            },
            PriceSnapshot::capture(
                Some(if free { (0.0, 0.0) } else { (1.0, 2.0) }),
                Default::default(),
                false,
            ),
        );
        if pending_at_start {
            session.spend_account.observe(&receipt).unwrap();
        }
        let (scanned_tx, scanned_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let query = Ledger::new(root.path()).with_summary_gate(scanned_tx, release_rx);
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
        let lines = scope
            .run(async {
                let mut scanned = crate::agent::runner::spawn_blocking_scoped(move || {
                    scanned_rx.recv_timeout(std::time::Duration::from_secs(3))
                });
                let mut report = Box::pin(super::cost_lines(&session, query));
                tokio::select! {
                    result = &mut scanned => {result.unwrap().unwrap();},
                    _ = &mut report => panic!("report must retain its earlier scan until released"),
                }
                if !pending_at_start {
                    session.spend_account.observe(&receipt).unwrap();
                }
                Ledger::new(root.path())
                    .flush(&session.spend_account)
                    .unwrap();
                assert!(
                    !session.spend_account.has_pending(),
                    "writer has ACKed before report's final account read"
                );
                release_tx.send(()).unwrap();
                report.await.unwrap()
            })
            .await;
        cleanup.settle().await;
        assert!(lines.iter().any(|line| line.contains(if pending_at_start {
            "Receipts were awaiting persistence when this query began"
        } else {
            "New usage was observed during this query"
        })));
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with("Observed receipts awaiting persistence")),
            "ACKed receipts must not be described as currently pending"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("Session ledger: $0.000000; input 0")),
            "never add a new in-memory receipt to an earlier durable subtotal"
        );
        assert!(lines.iter().any(|line| line.starts_with(if free {
            "Session recorded spend: $0.000000"
        } else {
            "Session recorded spend: $0.000070"
        })));
        assert_eq!(
            Ledger::new(root.path()).summary().unwrap().all_time.records,
            1
        );
        let current = super::cost_lines(&session, Ledger::new(root.path()))
            .await
            .unwrap();
        assert!(
            !current
                .iter()
                .any(|line| line.contains("awaiting persistence")
                    || line.contains("earlier persistence snapshot"))
        );
        assert!(current.iter().any(|line| line.contains("input 50")));
    }
}

#[cfg(feature = "subagents")]
#[tokio::test]
async fn background56_actual_task_and_signed_native_next_prompt_overlap_and_notify_once() {
    let root = crate::tests::OwnedTestDirectory::new("background56-native-overlap");
    let bindings = [
        "ZS_CONFIG_DIR",
        "ZS_DATA_DIR",
        "ZS_LOCAL_DATA_DIR",
        "ZS_STATE_DIR",
        "ZS_CACHE_DIR",
        "ZS_CREDENTIALS_DIR",
    ]
    .map(|name| (name, Some(root.path().join(name).into_os_string())));
    let _environment = crate::tests::ScopedProcessEnv::set(&bindings);
    std::fs::write(root.path().join("owned-child.txt"), "EXACT_BACKGROUND_READ").unwrap();
    let delta = serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"background-read","type":"function","function":{"name":"read","arguments":"{\"path\":\"owned-child.txt\"}"}}]});
    let first = serde_json::json!({"id":"child","object":"chat.completion.chunk","created":0,"model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let last = serde_json::json!({"id":"child","object":"chat.completion.chunk","created":0,"model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":50,"completion_tokens":10,"total_tokens":60}});
    let report = "## Findings\n- [confidence: high] EXACT_BACKGROUND_READ was read.\n\n## Unverified\n- None\n\n## Coverage\n- Covered: owned-child.txt.\n- Skipped: None.\n";
    let mut child = Wire::new(vec![
        Some(format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")),
        Some(format!("HOLD_GATE\n{}", response(report, false))),
    ])
    .await;
    let (child_client, child_cfg) = child.client();
    let mut priced_child = serde_json::to_value(child_cfg).unwrap();
    priced_child["quick_models"] = serde_json::json!({"background-child":{"provider":"cost-wire","model":"fixture","input_token_cost":1.0,"output_token_cost":2.0}});
    let child_cfg: Config = serde_json::from_value(priced_child).unwrap();
    let child_config = crate::extras::subagents::SubagentConfig {
        client: child_client,
        provider_name: "cost-wire".into(),
        session_provider: "cost-wire".into(),
        model_name: "fixture".into(),
        fallback_notice: None,
        api_key: Some("inert-child-key".into()),
        max_turns: 2,
        config: child_cfg.into(),
    };
    fn native(answer: &str, signature: &str) -> String {
        let events = [
            serde_json::json!({"type":"message_start","message":{"id":"native-parent","role":"assistant","content":[],"model":"claude-opus-5-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":signature}}),
            serde_json::json!({"type":"content_block_stop","index":0}),
            serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":answer}}),
            serde_json::json!({"type":"content_block_stop","index":1}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}),
            serde_json::json!({"type":"message_stop"}),
        ];
        let body = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }
    let mut parent = Wire::new(vec![
        Some(native("first answer", "opaque-signature-one")),
        Some(native("second answer", "opaque-signature-two")),
        Some(native("third answer", "opaque-signature-three")),
    ])
    .await;
    let cfg: Config = serde_json::from_value(serde_json::json!({"session_title_model":"off","compact_enabled":false,"retry":{"max_attempts":1},"custom_providers":{"background-native":{"provider_type":"anthropic","base_url":format!("http://{}",parent.address)}}})).unwrap();
    let cli = crate::cli::Cli {
        no_tools: true,
        no_context_files: true,
        no_sandbox: true,
        api_key: Some("inert-native-key".into()),
        ..Default::default()
    };
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = crate::session::Session::new(
        "background-native",
        "claude-opus-5-5",
        128000,
        "background proof",
    );
    session.working_dir = root.path().to_string_lossy().into();
    session.set_input_output_prices(Some((2.0, 4.0)));
    let sid = session.id.to_string();
    struct Release(String);
    impl Drop for Release {
        fn drop(&mut self) {
            crate::session::lock::release_session(&self.0);
        }
    }
    let _release = Release(sid.clone());
    let client = crate::provider::create_client(
        "background-native",
        cli.api_key.as_deref(),
        cfg.custom_providers_map(),
        None,
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
        crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace),
        None,
    );
    let snapshots = crate::startup::heap_future(|| {
        crate::ui::drive_native_background_fixture(
            ui,
            child_config,
            child.calls.clone(),
            child.release.clone(),
        )
    })
    .await;
    assert_eq!(parent.calls.load(Ordering::Acquire), 3);
    assert_eq!(child.calls.load(Ordering::Acquire), 2);
    {
        let requests = parent.bodies.lock().unwrap();
        assert_eq!(requests[0]["system"], requests[1]["system"]);
        assert_eq!(requests[1]["system"], requests[2]["system"]);
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        assert_eq!(requests[1]["tools"], requests[2]["tools"]);
        let mut initial_user = requests[0]["messages"][0].clone();
        assert_eq!(
            initial_user["content"][0]
                .as_object_mut()
                .unwrap()
                .remove("cache_control"),
            Some(serde_json::json!({"type":"ephemeral"}))
        );
        // Existing SDK moves its manual message cache marker to the latest user
        // boundary. Literal input and every opaque native block stay unchanged.
        assert_eq!(initial_user, requests[1]["messages"][0]);
        assert_eq!(
            requests[1]["messages"][1]["content"][0],
            serde_json::json!({"type":"thinking","thinking":"","signature":"opaque-signature-one"})
        );
        assert_eq!(requests[1]["messages"][1], requests[2]["messages"][1]);
        assert!(!requests[1].to_string().contains("[Background task "));
        assert_eq!(
            requests[2].to_string().matches("[Background task ").count(),
            1
        );
    }
    assert!(
        child.bodies.lock().unwrap()[1]
            .to_string()
            .contains("EXACT_BACKGROUND_READ")
    );
    assert!(snapshots[1].recorded_total_cost() > snapshots[0].recorded_total_cost());
    let rows = session.background_tasks();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].status,
        crate::extras::subagents::background::Status::Completed
    );
    assert_eq!(session.background_task_counts(), (0, 0));
    let reloaded = crate::session::storage::load_session_exact(&sid)
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.background_tasks()[0].status,
        crate::extras::subagents::background::Status::Completed
    );
    // Both exact loopback server tasks consumed their finite routes. Always
    // abort/join on test teardown too; no listener survives this fixture.
    parent.task.abort();
    child.task.abort();
    let _ = (&mut parent.task).await;
    let _ = (&mut child.task).await;
}
