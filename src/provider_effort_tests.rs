//! Native wire probes for effort/output-format composition and opaque thinking.

#[path = "provider_effort_replay_probe.rs"]
mod replay_probe;

#[path = "context/instruction_wire_tests.rs"]
mod instruction_wire_tests;

#[path = "provider_cache_tests.rs"]
mod cache_tests;

#[cfg(feature = "js")]
#[path = "provider_startup_tests.rs"]
mod startup_tests;

use super::*;
use crate::tests::support::loopback_http::blocking;
use futures::StreamExt;
use rig::completion::CompletionModel;
use serde_json::{Value, json};
use std::io::Read;
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
                let deadline = Instant::now() + Duration::from_secs(30);
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
                let request = blocking::read_request(&mut socket, 512 * 1024);
                assert!(
                    request.head.starts_with(&expected_request),
                    "{}",
                    request.head
                );
                requests.push(request.json());
                let content_type = if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                blocking::respond(&mut socket, "200 OK", content_type, &body);
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
            config.custom_providers_map(),
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

#[cfg(all(feature = "hooks", unix))]
fn hooks60_install_context_dispatcher(root: &std::path::Path, command: Option<String>) {
    use crate::extras::hooks::settings::{HookGroup, HookHandler, HookTrust, HooksConfig};
    let mut config = HooksConfig::new();
    for event in ["SessionStart", "UserPromptSubmit"] {
        let command = command.clone().unwrap_or_else(|| {
            format!("printf '%s' '{{\"additionalContext\":\"{event}_HOOK60_CONTEXT\"}}'")
        });
        config.insert(
            event.into(),
            vec![HookGroup {
                matcher: None,
                hooks: vec![HookHandler {
                    kind: "command".into(),
                    command: Some("/bin/sh".into()),
                    args: Some(vec!["-c".into(), command]),
                    timeout: Some(5),
                    is_async: false,
                    condition: None,
                    once: false,
                    trust: HookTrust::Trusted,
                    env: Default::default(),
                }],
            }],
        );
    }
    let dispatcher =
        crate::extras::hooks::dispatcher::HookDispatcher::from_config_with_backend_and_root(
            &config, "seatbelt", root,
        )
        .unwrap();
    crate::extras::hooks::init_dispatcher(dispatcher);
}

#[cfg(all(feature = "hooks", feature = "multimodal", unix))]
#[tokio::test]
async fn hooks60_actual_paused_gate_keeps_media_and_canonical_notice_before_start() {
    use crate::session::user::{ImageMime, PersistedUserMessage, UserBlock};
    let _guard = crate::tests::fake_model::dispatcher_guard::acquire();
    let root = crate::tests::OwnedTestDirectory::new("hooks60-paused");
    hooks60_install_context_dispatcher(root.path(), None);
    let user=PersistedUserMessage::new(vec![UserBlock::Text {text:"LITERAL_BEFORE60".into()},UserBlock::Image {data:"iVBORw0KGgoAAAANSUhEUgAAAAMAAAACCAIAAAASFvFNAAAAFElEQVR4nGPkSjnBAAZMEIqBgQEAFUoBOkiAy7EAAAAASUVORK5CYII=".into(),mime_type:ImageMime::Png},UserBlock::Text {text:"LITERAL_AFTER60".into()}]).unwrap();
    let original = user.to_message();
    let wire = Wire::new(vec![(
        true,
        start() + &text(0, "hook context observed") + &finish(),
    )]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Stable root policy")
            .max_tokens(100)
            .build(),
    ));
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let mut paused = crate::startup::heap_future(|| {
        agent.spawn_runner_paused_input_in_scope(
            crate::agent::input::PromptInput::user(user.clone()),
            Vec::<Message>::new(),
            RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            None,
            scope.clone(),
        )
    })
    .await;
    let notices = paused.take_hook_context();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("UserPromptSubmit_HOOK60_CONTEXT"));
    let mut session = crate::session::Session::new("native-test", "fixture", 32_000, "");
    crate::extras::hooks::append_context_session(&mut session, &notices);
    session.add_user_message(user);
    assert_eq!(
        session.messages[0].role,
        crate::session::MessageRole::System
    );
    assert_eq!(
        session.messages[1].user.as_ref().unwrap().to_message(),
        original
    );
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
    .expect("actual paused runner did not retire");
    cleanup.settle().await;
    assert!(done);
    let requests = wire.finish();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0]["system"].to_string().contains("HOOK60_CONTEXT"));
    let messages = requests[0]["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "assistant");
    assert!(messages[0].to_string().contains("HOOK60_CONTEXT"));
    assert_eq!(messages[1]["role"], "user");
    let parts = messages[1]["content"].as_array().unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0]["text"], "LITERAL_BEFORE60");
    assert_eq!(parts[2]["text"], "LITERAL_AFTER60");
    assert_eq!(parts[1]["type"], "image");
}

/// mini-agent-5eoy7.3: the Anthropic wire carries the host directive as the
/// last block of the current user turn, while UserPromptSubmit sees only the
/// literal text and the turn's committed interactions omit the prompt.
#[cfg(all(feature = "hooks", unix))]
#[tokio::test]
async fn turn_directive53_anthropic_wire_last_user_block_and_literal_hook_payload() {
    let _guard = crate::tests::fake_model::dispatcher_guard::acquire();
    let root = crate::tests::OwnedTestDirectory::new("turn-directive53");
    hooks60_install_context_dispatcher(
        root.path(),
        Some(
            "payload=$(cat); case \"$payload\" in *ROUTE_MARK53*) printf '%s' '{\"additionalContext\":\"HOOK_SAW_DIRECTIVE53\"}';; *LITERAL_MARK53*) printf '%s' '{\"additionalContext\":\"HOOK_SAW_LITERAL53\"}';; esac"
                .into(),
        ),
    );
    let wire = Wire::new(vec![(true, start() + &text(0, "routed") + &finish())]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Stable root policy")
            .max_tokens(100)
            .build(),
    ));
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let prompt = crate::agent::input::PromptInput::text("LITERAL_MARK53 question".into())
        .with_turn_directive(Some("ROUTE_MARK53 persona reviewer".into()));
    let mut paused = crate::startup::heap_future(|| {
        agent.spawn_runner_paused_input_in_scope(
            prompt,
            Vec::<Message>::new(),
            RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            None,
            scope.clone(),
        )
    })
    .await;
    let notices = paused.take_hook_context();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("HOOK_SAW_LITERAL53"), "{notices:?}");
    let mut runner = paused.start_interactive();
    let interactions = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Done { interactions, .. } => return interactions,
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
        panic!("runner closed without Done");
    })
    .await
    .expect("actual paused runner did not retire");
    cleanup.settle().await;
    assert!(
        !serde_json::to_string(&interactions)
            .unwrap()
            .contains("ROUTE_MARK53")
    );
    let requests = wire.finish();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0]["system"].to_string().contains("ROUTE_MARK53"));
    let messages = requests[0]["messages"].as_array().unwrap();
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "user");
    let parts = last["content"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["text"], "LITERAL_MARK53 question");
    let directive = parts[1]["text"].as_str().unwrap();
    assert!(directive.starts_with(crate::agent::input::TURN_DIRECTIVE_LABEL));
    assert!(directive.ends_with("ROUTE_MARK53 persona reviewer"));
    // Persisting the turn uses only the literal projection.
    let mut session = crate::session::Session::new("directive53", "fixture", 32_000, "");
    session.add_message(crate::session::MessageRole::User, "LITERAL_MARK53 question");
    assert!(
        !serde_json::to_string(&crate::agent::runner::convert_history(&session))
            .unwrap()
            .contains("ROUTE_MARK53")
    );
}

/// Stand-in `task` tool: the forcing hook keys on the advertised name only.
#[cfg(feature = "subagents")]
struct FakeTask55;
#[cfg(feature = "subagents")]
impl rig::tool::Tool for FakeTask55 {
    const NAME: &'static str = "task";
    type Error = std::io::Error;
    type Args = Value;
    type Output = String;
    fn description(&self) -> String {
        "Delegate to a specialist".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"agent_type":{"type":"string"}}})
    }
    async fn call(&self, _args: Value) -> Result<String, std::io::Error> {
        Ok("CHILD_REPORT55".into())
    }
}

/// A mention turn on the Anthropic wire: with the `task` tool the model calls
/// it, then answers after the tool result (two requests); without it the
/// model answers at once. Returns every request body.
#[cfg(feature = "subagents")]
async fn mention55_anthropic_turn(
    model_name: &str,
    first_request_tool: Option<&'static str>,
    with_task_tool: bool,
) -> Vec<Value> {
    #[cfg(feature = "hooks")]
    let _guard = crate::tests::fake_model::dispatcher_guard::acquire();
    let tool_call = start()
        + &event(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_task55","name":"task","input":{}}}),
        )
        + &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"agent_type\":\"reviewer\"}"}}),
        )
        + &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        )
        + &event(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
        )
        + &event("message_stop", json!({"type":"message_stop"}));
    let answer = start() + &text(0, "ANSWER55") + &finish();
    let wire = Wire::new(if with_task_tool {
        vec![(true, tool_call), (true, answer)]
    } else {
        vec![(true, answer)]
    });
    let AnyModel::Anthropic(model) = wire.client().completion_model(model_name) else {
        unreachable!()
    };
    let builder = rig::agent::AgentBuilder::new(model)
        .preamble("Stable root policy")
        .max_tokens(100)
        .default_max_turns(4);
    let agent = if with_task_tool {
        builder.tool(FakeTask55).build()
    } else {
        builder.build()
    };
    let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(agent));
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let prompt = crate::agent::input::PromptInput::text("please review".into())
        .with_turn_directive(Some(crate::context::mentions::routing_directive(&[
            "reviewer".into(),
        ])))
        .with_first_request_tool(first_request_tool);
    let paused = crate::startup::heap_future(|| {
        agent.spawn_runner_paused_input_in_scope(
            prompt,
            Vec::<Message>::new(),
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
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = runner.event_rx.recv().await {
            match event {
                crate::event::AgentEvent::Done { response, .. } => return response,
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
        panic!("runner closed without Done");
    })
    .await
    .expect("mention turn did not retire");
    cleanup.settle().await;
    assert!(response.contains("ANSWER55"), "{response}");
    let requests = wire.finish();
    assert_eq!(requests.len(), if with_task_tool { 2 } else { 1 });
    requests
}

/// rig's baseline for a tool-carrying Anthropic request is `auto` (or no
/// field); a forced choice is `tool`/`any`.
#[cfg(feature = "subagents")]
fn mention55_assert_not_forced(request: &Value) {
    let kind = request.pointer("/tool_choice/type");
    assert!(
        kind.is_none_or(|kind| kind == "auto"),
        "forced tool_choice: {}",
        request["tool_choice"]
    );
}

/// mini-agent-5eoy7.5 (3): `force` on a mention turn forces exactly the first
/// request to the `task` tool; the request carrying the tool result is not
/// forced, so the model can answer.
#[cfg(feature = "subagents")]
#[tokio::test]
async fn mention_tool_choice55_force_sets_task_on_first_request_only() {
    let requests = mention55_anthropic_turn("claude-opus-4-8", Some("task"), true).await;
    assert_eq!(
        requests[0]["tool_choice"],
        json!({"type":"tool","name":"task"})
    );
    assert!(requests[0]["tools"].to_string().contains("\"task\""));
    mention55_assert_not_forced(&requests[1]);
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("CHILD_REPORT55")
    );
}

/// `hint` (no typed marker) never carries `tool_choice`, on either request.
#[cfg(feature = "subagents")]
#[tokio::test]
async fn mention_tool_choice55_hint_turn_sends_no_tool_choice() {
    let requests = mention55_anthropic_turn("claude-opus-4-8", None, true).await;
    for request in &requests {
        mention55_assert_not_forced(request);
    }
}

/// A request path that cannot carry a forced tool (Claude Opus 5.5 rejects
/// forced tool_choice and thinks by default) and a request whose tool list
/// lacks `task` both fall back to the hint and still complete.
#[cfg(feature = "subagents")]
#[tokio::test]
async fn mention_tool_choice55_unsupported_provider_or_missing_tool_falls_back_to_hint() {
    let requests = mention55_anthropic_turn("claude-opus-5-5", Some("task"), true).await;
    for request in &requests {
        mention55_assert_not_forced(request);
    }
    let requests = mention55_anthropic_turn("claude-opus-4-8", Some("task"), false).await;
    mention55_assert_not_forced(&requests[0]);
}

/// The Claude capability table: forcing only with thinking known off and
/// never on models that reject forced tools; non-Claude OpenRouter routes
/// carry it.
#[cfg(feature = "subagents")]
#[test]
fn mention_tool_choice55_claude_blocker_table() {
    use effort::Protocol;
    assert_eq!(
        claude_forced_tool_choice_blocker(Protocol::Anthropic, "claude-opus-4-8", None),
        None
    );
    assert!(
        claude_forced_tool_choice_blocker(
            Protocol::Anthropic,
            "claude-opus-4-8",
            Some(&json!({"thinking":{"type":"adaptive"}}))
        )
        .is_some()
    );
    for model in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-1"] {
        assert!(claude_forced_tool_choice_blocker(Protocol::Anthropic, model, None).is_some());
    }
    assert!(
        claude_forced_tool_choice_blocker(Protocol::Anthropic, "claude-unknown-model", None)
            .is_some()
    );
    assert_eq!(
        claude_forced_tool_choice_blocker(Protocol::OpenRouter, "openai/gpt-5", None),
        None
    );
    assert!(
        claude_forced_tool_choice_blocker(Protocol::OpenRouter, "anthropic/claude-opus-5.5", None)
            .is_some()
    );
}

#[cfg(all(feature = "hooks", unix))]
#[tokio::test]
async fn hooks60_actual_headless_gate_commits_system_notices_and_native_feed() {
    use crate::session::native_replay::{ReplayBoundary, RetiredNativeOutcome};
    let _guard = crate::tests::fake_model::dispatcher_guard::acquire();
    let root = crate::tests::OwnedTestDirectory::new("hooks60-native");
    hooks60_install_context_dispatcher(root.path(), None);
    let notices = crate::extras::hooks::dispatch_session_start_with(
        &crate::extras::hooks::get_dispatcher().unwrap(),
        &crate::extras::hooks::HookCtx {
            session_id: "hook60".into(),
            session_path: String::new(),
            cwd: root.path().display().to_string(),
            permission_mode: "standard".into(),
        },
        "startup",
    )
    .await
    .into_notices();
    assert_eq!(notices.len(), 1);
    let signed = start()
        + &event(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        )
        + &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"HOOK60_NATIVE_SIGNATURE"}}),
        )
        + &event(
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        )
        + &text(1, "native hook context accepted")
        + &finish();
    let wire = Wire::new(vec![(true, signed)]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let mut session = crate::session::Session::new("native-test", "claude-opus-5-5", 32_000, "");
    crate::extras::hooks::append_context_session(&mut session, &notices);
    let prices = crate::print::HeadlessPricing::from_session(&session, &Config::default());
    let generic = crate::agent::runner::convert_history(&session);
    let (agent, binding) = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("Stable native root policy")
            .max_tokens(100)
            .build(),
    ))
    .bind_native_turn(
        &session,
        generic.into(),
        ReplayBoundary::CompletedTurn,
        RecordedToolIds::Canonical,
    )
    .unwrap();
    let retry = RetryConfig {
        max_attempts: 1,
        ..Default::default()
    };
    let turn = crate::startup::heap_future(|| {
        agent.run_print_with_output(
            "EXACT_LITERAL_HOOK60",
            crate::print::HeadlessOutputPolicy::legacy(false, false),
            &retry,
            binding.history(),
            None,
        )
    })
    .await;
    assert!(turn.failure.is_none(), "{:?}", turn.failure);
    assert!(matches!(
        turn.interactions.first(),
        Some(Message::System { .. })
    ));
    assert_eq!(
        binding.commit_canonical_into_with_outcome(
            &mut session,
            "EXACT_LITERAL_HOOK60",
            &turn.response,
            &turn.interactions,
            turn.usage.into(),
            prices,
            RetiredNativeOutcome::Completed
        ),
        Ok(true)
    );
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|row| row.role == crate::session::MessageRole::System)
            .count(),
        2
    );
    let row = session
        .messages
        .iter()
        .find(|row| row.role == crate::session::MessageRole::User)
        .unwrap();
    assert_eq!(row.content, "EXACT_LITERAL_HOOK60");
    let restored =
        crate::session::storage::decode_session_json(&serde_json::to_string(&session).unwrap())
            .unwrap();
    let replay = restored
        .native_restore_history(
            crate::session::native_replay::HistoryProtocol::AnthropicMessages,
            ReplayBoundary::CompletedTurn,
        )
        .unwrap();
    let bytes = serde_json::to_string(&replay).unwrap();
    assert!(bytes.contains("HOOK60_NATIVE_SIGNATURE"));
    assert!(bytes.contains("UserPromptSubmit_HOOK60_CONTEXT"));
    assert!(bytes.contains("SessionStart_HOOK60_CONTEXT"));
    let requests = wire.finish();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0]["system"].to_string().contains("HOOK60_CONTEXT"));
    assert!(
        requests[0]["messages"]
            .to_string()
            .contains("UserPromptSubmit_HOOK60_CONTEXT")
    );
}

#[cfg(all(feature = "hooks", unix))]
#[tokio::test]
async fn hooks60_actual_paused_gate_cancellation_retires_hook_before_zero_http() {
    use crate::tests::process_state::{ProcessIdentity, ProcessState};
    let _guard = crate::tests::fake_model::dispatcher_guard::acquire();
    let root = crate::tests::OwnedTestDirectory::new("hooks60-cancel");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let pid = root.path().join("hook.pid");
    let operand = format!("'{}'", pid.to_str().unwrap().replace('\'', "'\\''"));
    let command = format!("printf '%s\\n' \"$$\" > {}; exec /bin/sleep 30", operand);
    hooks60_install_context_dispatcher(&workspace, Some(command));
    let wire = Wire::new(Vec::new());
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let agent = AnyAgent::without_skills(AnyAgentInner::Anthropic(
        rig::agent::AgentBuilder::new(model)
            .preamble("stable cancellation control")
            .max_tokens(100)
            .build(),
    ));
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let wait = async {
        let identity = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(value) = tokio::fs::read_to_string(&pid).await
                    && value.ends_with('\n')
                    && let Ok(pid) = value.trim().parse::<u32>()
                {
                    break ProcessIdentity::capture(pid).unwrap();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual configured hook never started");
        scope.cancellation_handle().cancel();
        identity
    };
    let constructing = crate::startup::heap_future(|| {
        agent.spawn_runner_paused_input_in_scope(
            crate::agent::input::PromptInput::text("cancel actual hook".into()),
            Vec::<Message>::new(),
            RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            None,
            scope.clone(),
        )
    });
    let (mut paused, identity) = tokio::join!(constructing, wait);
    assert!(paused.take_hook_context().is_empty());
    let mut runner = paused.start_interactive();
    tokio::time::timeout(Duration::from_secs(5), async {
        while runner.event_rx.recv().await.is_some() {}
    })
    .await
    .unwrap();
    cleanup.settle().await;
    assert!(matches!(
        identity.state().unwrap(),
        ProcessState::Gone | ProcessState::Exited | ProcessState::Replaced
    ));
    assert!(wire.finish().is_empty());
}
