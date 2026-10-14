//! Native wire probes for effort/output-format composition and opaque thinking.

#[path = "provider_effort_replay_probe.rs"]
mod replay_probe;

#[path = "context/instruction_wire_tests.rs"]
mod instruction_wire_tests;

#[path = "provider_cache_tests.rs"]
mod cache_tests;

use super::*;
use futures::StreamExt;
use rig::completion::CompletionModel;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct Wire {
    base: String,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<Vec<Value>>>,
}

impl Wire {
    fn new(responses: Vec<(bool, String)>) -> Self {
        Self::at_path(responses, "/v1/messages")
    }

    fn at_path(responses: Vec<(bool, String)>, path: &str) -> Self {
        let expected_request = format!("POST {path} HTTP/1.1\r\n");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stop);
        let join = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let mut responses = responses.into_iter();
            loop {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if cancelled.load(Ordering::Acquire) {
                                return requests;
                            }
                            assert!(Instant::now() < deadline, "native probe accept deadline");
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("native probe accept: {error}"),
                    }
                };
                let Some((stream, body)) = responses.next() else {
                    // Keep even a zero-response endpoint live until finish.
                    // Drain accepted TCP attempts before honoring cancellation.
                    requests.push(json!({"unexpected_tcp_attempt": true}));
                    return requests;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    assert!(headers.len() < 64 * 1024);
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                let headers = String::from_utf8(headers).unwrap();
                assert!(headers.starts_with(&expected_request), "{headers}");
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                assert!(length <= 512 * 1024);
                let mut bytes = vec![0; length];
                socket.read_exact(&mut bytes).unwrap();
                requests.push(serde_json::from_slice(&bytes).unwrap());
                let content_type = if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                socket.flush().unwrap();
            }
        });
        Self {
            base,
            stop,
            join: Some(join),
        }
    }

    fn client(&self) -> AnyClient {
        self.client_for("anthropic")
    }

    fn client_for(&self, provider_type: &str) -> AnyClient {
        let config: Config = serde_json::from_value(json!({
            "custom_providers":{"effort-probe":{"provider_type":provider_type,"base_url":self.base}}
        }))
        .unwrap();
        create_client(
            "effort-probe",
            Some("effort-fake-key"),
            &config.custom_providers_map(),
            None,
        )
        .unwrap()
    }

    fn finish(mut self) -> Vec<Value> {
        self.stop.store(true, Ordering::Release);
        self.join.take().unwrap().join().unwrap()
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn event(kind: &str, body: Value) -> String {
    format!("event: {kind}\ndata: {body}\n\n")
}

fn start() -> String {
    event(
        "message_start",
        json!({"type":"message_start","message":{"id":"msg_probe","type":"message","role":"assistant","content":[],"model":"claude-opus-5-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
    )
}

fn finish() -> String {
    event(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}),
    ) + &event("message_stop", json!({"type":"message_stop"}))
}

fn text(index: usize, text: &str) -> String {
    event(
        "content_block_start",
        json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
    ) + &event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":index}),
    )
}

fn signed(index: usize, signature: &str, stopped: bool) -> String {
    let mut body = event(
        "content_block_start",
        json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":"","signature":""}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":""}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":signature}}),
    );
    if stopped {
        body.push_str(&event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":index}),
        ));
    }
    body
}

#[tokio::test]
async fn effort46_native_output_config_merges_owned_schema_on_stream_and_completion_wire() {
    for stream in [true, false] {
        let response = if stream {
            start() + &text(0, "7") + &finish()
        } else {
            json!({"id":"msg_probe","type":"message","role":"assistant","content":[{"type":"text","text":"7"}],"model":"claude-opus-5-5","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":2}}).to_string()
        };
        let wire = Wire::new(vec![(stream, response)]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let mut request = model.completion_request("return integer").max_tokens(1000)
            .additional_params(json!({"thinking":{"type":"adaptive"},"output_config":{"effort":"max","format":{"type":"json_schema","schema":{"type":"string"}}}}))
            .build();
        request.output_schema = Some(serde_json::from_value(json!({"type":"integer"})).unwrap());
        if stream {
            let mut response = model.stream(request).await.unwrap();
            while let Some(part) = response.next().await {
                part.unwrap();
            }
        } else {
            model.completion(request).await.unwrap();
        }
        let requests = wire.finish();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(
            request["output_config"],
            json!({"effort":"max","format":{"type":"json_schema","schema":{"type":"integer"}}})
        );
        assert_eq!(request["max_tokens"], 1000);
        assert_eq!(request["thinking"], json!({"type":"adaptive"}));
    }
}

#[tokio::test]
async fn effort46_native_zero_tool_signed_and_interrupted_blocks_are_distinct() {
    for (signature, stopped, expected) in [
        ("opaque-native-signature", true, 1),
        ("", true, 0),
        ("partial-signature", false, 0),
    ] {
        let mut response = start() + &signed(0, signature, stopped);
        if stopped {
            response.push_str(&(text(1, "done") + &finish()));
        }
        let wire = Wire::new(vec![(true, response)]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let mut response = model
            .completion_request("answer without tools")
            .max_tokens(1000)
            .stream()
            .await
            .unwrap();
        while let Some(part) = response.next().await {
            part.unwrap();
        }
        let reasoning: Vec<_> = response
            .choice
            .iter()
            .filter_map(|item| match item {
                rig::message::AssistantContent::Reasoning(value) => Some(value),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning.len(), expected);
        if expected == 1 {
            assert!(
                matches!(reasoning[0].content.first(), Some(rig::message::ReasoningContent::Text { text, signature: Some(value) }) if text.is_empty() && value == signature)
            );
        }
        assert_eq!(wire.finish().len(), 1);
    }
}

#[tokio::test]
async fn effort46_native_invalid_effort_fails_before_http_without_echoing_values() {
    let wire = Wire::new(Vec::new());
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    for (output_config, expected) in [
        (
            json!("PRIVATE_EFFORT_CANARY"),
            "Anthropic output_config must be an object",
        ),
        (
            json!({"effort":42}),
            "invalid Anthropic output_config.effort",
        ),
        (
            json!({"effort":"PRIVATE_EFFORT_CANARY"}),
            "invalid Anthropic output_config.effort",
        ),
    ] {
        let error = model
            .completion_request("unused")
            .max_tokens(1000)
            .additional_params(json!({"output_config":output_config}))
            .stream()
            .await
            .err()
            .unwrap();
        let rig::completion::CompletionError::RequestError(error) = error else {
            panic!("expected a local request error, got {error:?}");
        };
        assert_eq!(error.to_string(), expected);
    }
    assert!(wire.finish().is_empty());
}

#[tokio::test]
async fn effort46_native_effort_only_and_absent_controls_keep_wire_shape() {
    let wire = Wire::new(vec![(true, start() + &text(0, "done") + &finish()); 2]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    for params in [None, Some(json!({"output_config":{"effort":"medium"}}))] {
        let mut request = model.completion_request("answer").max_tokens(1000).build();
        request.additional_params = params;
        let mut response = model.stream(request).await.unwrap();
        while let Some(part) = response.next().await {
            part.unwrap();
        }
    }
    let requests = wire.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].get("output_config").is_none());
    assert_eq!(requests[1]["output_config"], json!({"effort":"medium"}));
    assert!(requests.iter().all(|request| request["max_tokens"] == 1000));
}

#[cfg(feature = "agent-skills")]
#[tokio::test]
async fn skill61_native_search_keeps_signed_system_prefix_and_effective_result_context() {
    for rewritten in [None, Some("redacted"), Some("[Tool output externalized]")] {
        use crate::extras::skills::session::{
            ImmutableFixtureCleanup, InstructionOwner, InstructionTurn,
        };
        let owned = crate::tests::OwnedTestDirectory::new("native-skill61-prefix");
        let _cleanup = ImmutableFixtureCleanup(owned.path().to_path_buf());
        let paths = crate::paths::AppPaths {
            config_dir: owned.path().join("config"),
            data_dir: owned.path().join("data"),
            local_data_dir: owned.path().join("local"),
            state_dir: owned.path().join("state"),
            cache_dir: owned.path().join("cache"),
            credentials_dir: owned.path().join("credentials"),
            project_dir: None,
        };
        crate::paths::prepare_storage_roots(&paths).unwrap();
        let package = owned.path().join("reviewer");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(
            package.join("SKILL.md"),
            "---\nname: reviewer\ndescription: code review\n---\nNATIVE_SEARCH_GUIDANCE_雪\n",
        )
        .unwrap();
        crate::extras::skills::import_agent_skill(&package, &paths).unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(owned.path()).unwrap());
        let services = InstructionOwner::for_test(None).resolve(paths, workspace, None, true);
        let initial = start()
            + &event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            )
            + &event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"native-skill-signature"}}),
            )
            + &event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            )
            + &event(
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"native-skill-redacted"}}),
            )
            + &event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            )
            + &event(
                "content_block_start",
                json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_skill_search","name":"skills_search","input":{}}}),
            )
            + &event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"code review\"}"}}),
            )
            + &event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            )
            + &event(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
            )
            + &event("message_stop", json!({"type":"message_stop"}));
        let wire = Wire::new(vec![
            (true, initial),
            (true, start() + &text(0, "NATIVE_DONE") + &finish()),
        ]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let inner = rig::agent::AgentBuilder::new(model)
            .preamble(&format!(
                "STATIC_NATIVE_SKILL_POLICY{}",
                crate::extras::skills::selection::POLICY
            ))
            .max_tokens(1000)
            .default_max_turns(4)
            .tool(crate::extras::skills::search_tool::SkillsSearchTool::new(
                services.clone(),
            ))
            .build();
        let captured = InstructionTurn::new(services.clone(), None);
        let mut agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(inner))
            .with_instruction_services(services.clone())
            .with_instruction_turn(Some(captured.clone()));
        if let Some(text) = rewritten {
            agent = agent.with_instruction_test_result_rewrite(text);
        }
        let turn = agent
            .run_print_with_output(
                "unrelated initial topic",
                crate::print::HeadlessOutputPolicy::legacy(false, true),
                &RetryConfig {
                    max_attempts: 1,
                    ..Default::default()
                },
                Vec::<rig::completion::Message>::new(),
                #[cfg(feature = "hooks")]
                None,
            )
            .await;
        let requests = wire.finish();
        if let Some(text) = rewritten {
            assert!(
                turn.failure
                    .as_ref()
                    .is_some_and(|error| error.to_string().contains("rewritten or removed")),
                "{:?}",
                turn.failure
            );
            assert_eq!(
                requests.len(),
                1,
                "rewritten native context must stop before another HTTP"
            );
            assert!(
                captured.prepare("retry").await.unwrap().selected.is_empty(),
                "rewritten result cannot adopt a candidate"
            );
            assert!(format!("{:?}", turn.interactions).contains(text));
            assert_eq!(turn.usage.input_tokens, 10);
            assert_eq!(turn.usage.output_tokens, 2);
        } else {
            assert!(turn.failure.is_none(), "{:?}", turn.failure);
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[0]["system"], requests[1]["system"],
                "signed native prefix cannot change after skills_search"
            );
            assert_eq!(requests[0]["tools"], requests[1]["tools"]);
            let second = serde_json::to_string(&requests[1]["messages"]).unwrap();
            assert!(
                second.contains("native-skill-signature")
                    && second.contains("native-skill-redacted")
            );
            assert!(second.contains("NATIVE_SEARCH_GUIDANCE_雪"));
            assert_eq!(turn.usage.input_tokens, 20);
            assert_eq!(turn.usage.output_tokens, 4);
        }
    }
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn skill61_combined_search_does_not_publish_learned_bundle_before_intact_result() {
    use crate::extras::js::skills::session::SkillSessionServices;
    use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
    use crate::extras::skills::session::{
        ImmutableFixtureCleanup, InstructionContextHook, InstructionOwner, InstructionTurn,
    };
    use rig::tool::Tool;
    struct HeldSearch {
        tool: crate::extras::js::skills::search_tool::SkillsSearchTool,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl Tool for HeldSearch {
        const NAME: &'static str = "skills_search";
        type Args = crate::extras::js::skills::search_tool::SkillsSearchArgs;
        type Output = String;
        type Error = crate::agent::tools::ToolError;
        fn description(&self) -> String {
            self.tool.description()
        }
        fn parameters(&self) -> Value {
            self.tool.parameters()
        }
        async fn call(&self, args: Self::Args) -> Result<String, Self::Error> {
            self.tool.call(args).await
        }
        async fn call_structured(
            &self,
            args: Self::Args,
            ext: &rig::tool::ToolCallExtensions,
        ) -> Result<rig::tool::ToolReturn<String>, Self::Error> {
            let result = self.tool.call_structured(args, ext).await?;
            self.entered.notify_one();
            self.release.notified().await;
            Ok(result)
        }
    }
    struct ObserveBundle {
        services: Arc<SkillSessionServices>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        observed: Arc<std::sync::Mutex<Option<crate::extras::js::skills::turn::TurnSkillBundle>>>,
    }
    impl Tool for ObserveBundle {
        const NAME: &'static str = "observe_bundle";
        type Args = Value;
        type Output = String;
        type Error = std::io::Error;
        fn description(&self) -> String {
            "Observe the actual captured bundle; no external effects".into()
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        async fn call(&self, _: Value) -> Result<String, Self::Error> {
            self.entered.notified().await;
            *self.observed.lock().unwrap() =
                Some((*self.services.turn_context().snapshot()).clone());
            self.release.notify_one();
            Ok("captured bundle observed".into())
        }
    }
    for rewrite in [None, Some("redacted"), Some("[Tool output externalized]")] {
        let owned = crate::tests::OwnedTestDirectory::new("combined-skill61");
        let _cleanup = ImmutableFixtureCleanup(owned.path().to_path_buf());
        let paths = crate::paths::AppPaths {
            config_dir: owned.path().join("config"),
            data_dir: owned.path().join("data"),
            local_data_dir: owned.path().join("local"),
            state_dir: owned.path().join("state"),
            cache_dir: owned.path().join("cache"),
            credentials_dir: owned.path().join("credentials"),
            project_dir: None,
        };
        crate::paths::prepare_storage_roots(&paths).unwrap();
        let artifact = SkillArtifact::new(
            "function stagedReview(_cap,value) { return value.trim(); }".into(),
            "Code review staged callable".into(),
            vec!["code".into(), "review".into()],
            vec![SkillExport {
                name: "stagedReview".into(),
                signature: "stagedReview(value: string): string".into(),
            }],
            vec!["stagedReview(' x ') === 'x'".into()],
            CapabilityManifest::pure(),
        )
        .unwrap();
        crate::extras::js::skills::store::SkillStore::open_at(&paths)
            .and_then(|mut store| store.insert_verified(&artifact))
            .unwrap();
        let learned = SkillSessionServices::for_test_with_learned(&paths).await;
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(owned.path()).unwrap());
        let services = InstructionOwner::for_test(None).resolve(paths, workspace, None, true);
        let captured = InstructionTurn::new(services, None);
        learned
            .prepare_instruction_prompt("unrelated initial topic", &captured)
            .await
            .unwrap();
        let before = (*learned.turn_context().snapshot()).clone();
        assert!(before.skills.is_empty());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let observed = Arc::new(std::sync::Mutex::new(None));
        let initial = start()
            + &event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            )
            + &event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"combined-signature"}}),
            )
            + &event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            )
            + &event(
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"combined-search","name":"skills_search","input":{}}}),
            )
            + &event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"code review\"}"}}),
            )
            + &event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            )
            + &event(
                "content_block_start",
                json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"combined-observe","name":"observe_bundle","input":{}}}),
            )
            + &event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            )
            + &event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            )
            + &event(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
            )
            + &event("message_stop", json!({"type":"message_stop"}));
        let wire = Wire::new(vec![
            (true, initial),
            (true, start() + &text(0, "COMBINED_DONE") + &finish()),
        ]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let base = format!("COMBINED_BASE{}", crate::extras::skills::selection::POLICY);
        let inner = rig::agent::AgentBuilder::new(model)
            .preamble(&base)
            .max_tokens(1000)
            .default_max_turns(4)
            .tool(HeldSearch {
                tool: crate::extras::js::skills::search_tool::SkillsSearchTool::new(
                    learned.clone(),
                ),
                entered: entered.clone(),
                release: release.clone(),
            })
            .tool(ObserveBundle {
                services: learned.clone(),
                entered,
                release,
                observed: observed.clone(),
            })
            .build();
        let mut agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(inner));
        if let Some(text) = rewrite {
            agent = agent.with_instruction_test_result_rewrite(text);
        }
        // Production appends its captured compositor after ordinary rewriting.
        let AnyAgentInner::Anthropic(inner) = &mut agent.inner else {
            unreachable!()
        };
        inner.hooks.push(InstructionContextHook::new(
            base,
            captured.clone(),
            "unrelated initial topic".into(),
            crate::context::instructions::InstructionProtocol::Anthropic,
            Some(learned.clone()),
        ));
        let turn = tokio::time::timeout(
            Duration::from_secs(10),
            agent.run_print_with_output(
                "unrelated initial topic",
                crate::print::HeadlessOutputPolicy::legacy(false, true),
                &RetryConfig {
                    max_attempts: 1,
                    ..Default::default()
                },
                Vec::<rig::completion::Message>::new(),
                #[cfg(feature = "hooks")]
                None,
            ),
        )
        .await
        .unwrap();
        let requests = wire.finish();
        assert_eq!(
            observed.lock().unwrap().as_ref(),
            Some(&before),
            "same-batch runtime consumer must not observe an unacknowledged candidate"
        );
        let after = learned.turn_context().snapshot();
        if rewrite.is_some() {
            assert_eq!(*after, before);
            assert_eq!(requests.len(), 1);
            assert!(turn.failure.is_some());
        } else {
            assert_eq!(after.turn_id, before.turn_id);
            assert!(after.skills.iter().any(|skill| {
                skill.id == artifact.id
                    && skill
                        .exports
                        .iter()
                        .any(|export| export.name == "stagedReview")
            }));
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0]["system"], requests[1]["system"]);
            assert!(
                serde_json::to_string(&requests[1]["messages"])
                    .unwrap()
                    .contains("stagedReview")
            );
            assert!(turn.failure.is_none());
        }
    }
}
