//! Native wire probes for effort/output-format composition and opaque thinking.

#[path = "provider_effort_replay_probe.rs"]
mod replay_probe;

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
                assert!(headers.starts_with("POST /v1/messages HTTP/1.1\r\n"));
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
        let config: Config = serde_json::from_value(json!({
            "custom_providers":{"effort-probe":{"provider_type":"anthropic","base_url":self.base}}
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
