//! Detection of and recovery from provider output-token truncation.
//!
//! A provider that stops a response at its output-token limit (Anthropic
//! `stop_reason: max_tokens`, OpenAI `finish_reason: length`, Responses
//! `incomplete: max_output_tokens`, Gemini `MAX_TOKENS`, Ollama
//! `done_reason: length`) used to look like an ordinary end of turn: a long
//! answer was cut silently and a large tool-call payload surfaced as an opaque
//! JSON error. The patched Rig SDK reports the stop on each `CompletionCall`
//! (`output_truncated`) and turns a cut-off tool call into an error carrying
//! [`rig::completion::OUTPUT_TRUNCATED_ERROR_MARKER`]; the turn loops use
//! [`OutputTruncation`] to emit a visible notice and continue the turn with an
//! explicit instruction, at most [`MAX_TRUNCATION_CONTINUATIONS`] times.

/// Automatic continuations after truncation within one user turn. Beyond
/// this the notice is still shown, and the turn ends (or fails for a cut-off
/// tool call) instead of looping on a model that cannot fit its output.
pub(crate) const MAX_TRUNCATION_CONTINUATIONS: u32 = 3;

/// What the turn loop should do about a detected truncation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TruncationDecision {
    /// Show `notice`, then continue the turn with `instruction`.
    Continue { notice: String, instruction: String },
    /// Show `notice`; the continuation budget is spent.
    Exhausted { notice: String },
}

impl TruncationDecision {
    pub(crate) fn notice(&self) -> &str {
        match self {
            Self::Continue { notice, .. } | Self::Exhausted { notice } => notice,
        }
    }
}

/// Per-turn truncation state shared by the interactive and headless loops.
#[derive(Debug, Default)]
pub(crate) struct OutputTruncation {
    last_call_truncated: bool,
    last_output_tokens: u64,
    continuations: u32,
}

impl OutputTruncation {
    /// Record the latest provider call's terminal metadata.
    pub(crate) fn observe_call(&mut self, call: &rig::agent::CompletionCall) {
        self.last_call_truncated = call.output_truncated;
        self.last_output_tokens = call.usage.output_tokens;
    }

    /// The terminal answer is complete unless the last call was truncated.
    pub(crate) fn final_response(&mut self) -> Option<TruncationDecision> {
        if !std::mem::take(&mut self.last_call_truncated) {
            return None;
        }
        let tokens = self.tokens_phrase();
        let notice =
            format!("output truncated: the response was cut off at the output-token limit{tokens}");
        Some(self.decide(
            notice,
            format!(
                "[output truncated] Your previous response was cut off at the output-token \
                 limit (max_tokens){tokens}. Continue exactly where it stopped, without \
                 repeating text you already wrote. If you were producing a large file or \
                 document, write it in smaller pieces."
            ),
        ))
    }

    /// A stream error that reports a cut-off tool call, or `None` for any
    /// other error.
    pub(crate) fn stream_error(&mut self, error: &str) -> Option<TruncationDecision> {
        if !is_output_truncation_error(error) {
            return None;
        }
        self.last_call_truncated = false;
        let tool = tool_name(error)
            .map(|name| format!(" for tool call `{name}`"))
            .unwrap_or_default();
        let notice = format!(
            "output truncated: the model hit the output-token limit while writing arguments{tool}; the call was not executed"
        );
        Some(self.decide(
            notice,
            format!(
                "[output truncated] Your previous response hit the output-token limit \
                 (max_tokens) while writing tool-call arguments{tool}, so that call was NOT \
                 executed. Redo it in smaller steps: split large file contents across \
                 several smaller write or edit calls instead of one large payload."
            ),
        ))
    }

    fn decide(&mut self, notice: String, instruction: String) -> TruncationDecision {
        if self.continuations < MAX_TRUNCATION_CONTINUATIONS {
            self.continuations += 1;
            TruncationDecision::Continue {
                notice: format!(
                    "{notice}; continuing ({}/{MAX_TRUNCATION_CONTINUATIONS})",
                    self.continuations
                ),
                instruction,
            }
        } else {
            TruncationDecision::Exhausted {
                notice: format!(
                    "{notice}; automatic continuation limit ({MAX_TRUNCATION_CONTINUATIONS}) reached. \
                     Raise `max_tokens` (or `--max-tokens`) or ask for smaller steps."
                ),
            }
        }
    }

    fn tokens_phrase(&self) -> String {
        if self.last_output_tokens > 0 {
            format!(" after {} output tokens", self.last_output_tokens)
        } else {
            String::new()
        }
    }
}

/// Whether a provider/stream error means the output-token limit cut the
/// response: the SDK's cut-off tool-call marker, or an OpenAI Responses
/// `response.incomplete` with reason `max_output_tokens`.
pub(crate) fn is_output_truncation_error(error: &str) -> bool {
    error.contains(rig::completion::OUTPUT_TRUNCATED_ERROR_MARKER)
        || (error.contains("max_output_tokens") && error.contains("incomplete"))
}

fn tool_name(error: &str) -> Option<&str> {
    let start = error.find("tool call `")? + "tool call `".len();
    let len = error[start..].find('`')?;
    let name = &error[start..start + len];
    (!name.is_empty() && name.len() <= 128).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(truncated: bool, output_tokens: u64) -> rig::agent::CompletionCall {
        let mut usage = rig::completion::Usage::new();
        usage.output_tokens = output_tokens;
        rig::agent::CompletionCall::new(0, usage).with_output_truncated(truncated)
    }

    #[test]
    fn complete_final_response_needs_no_action() {
        let mut state = OutputTruncation::default();
        state.observe_call(&call(false, 10));
        assert_eq!(state.final_response(), None);
    }

    #[test]
    fn truncated_final_response_continues_then_exhausts() {
        let mut state = OutputTruncation::default();
        for attempt in 1..=MAX_TRUNCATION_CONTINUATIONS {
            state.observe_call(&call(true, 4096));
            let Some(TruncationDecision::Continue {
                notice,
                instruction,
            }) = state.final_response()
            else {
                panic!("attempt {attempt} must continue");
            };
            assert!(notice.contains("after 4096 output tokens"), "{notice}");
            assert!(notice.contains(&format!("({attempt}/")), "{notice}");
            assert!(instruction.contains("Continue exactly where it stopped"));
        }
        state.observe_call(&call(true, 4096));
        let decision = state.final_response().unwrap();
        assert!(matches!(decision, TruncationDecision::Exhausted { .. }));
        assert!(decision.notice().contains("Raise `max_tokens`"));
    }

    #[test]
    fn truncated_tool_call_error_names_the_tool() {
        let mut state = OutputTruncation::default();
        let error = format!(
            "CompletionError: ResponseError: {}: the arguments of tool call `write` were cut off and the call was not executed",
            rig::completion::OUTPUT_TRUNCATED_ERROR_MARKER
        );
        let Some(TruncationDecision::Continue {
            notice,
            instruction,
        }) = state.stream_error(&error)
        else {
            panic!("tool truncation must continue");
        };
        assert!(notice.contains("`write`"), "{notice}");
        assert!(instruction.contains("NOT executed"));
        assert_eq!(state.stream_error("connection reset by peer"), None);
    }

    #[test]
    fn responses_incomplete_max_output_tokens_is_truncation() {
        assert!(is_output_truncation_error(
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#
        ));
        assert!(!is_output_truncation_error(
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#
        ));
    }

    /// A local HTTP fixture answering each request with the next scripted
    /// SSE body and recording the request bodies.
    async fn sse_server(bodies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for body in bodies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    headers.push(socket.read_u8().await.unwrap());
                }
                let headers = String::from_utf8(headers).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut request = vec![0; length];
                socket.read_exact(&mut request).await.unwrap();
                seen.push(String::from_utf8(request).unwrap());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            seen
        });
        (url, handle)
    }

    fn anthropic_text_turn(text: &str, stop_reason: &str) -> String {
        let event =
            |kind: &str, data: serde_json::Value| format!("event: {kind}\ndata: {data}\n\n");
        [
            event(
                "message_start",
                serde_json::json!({"type":"message_start","message":{"id":"msg","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":1}}}),
            ),
            event(
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            ),
            event(
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":0}),
            ),
            event(
                "message_delta",
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":{"output_tokens":64}}),
            ),
            event("message_stop", serde_json::json!({"type":"message_stop"})),
        ]
        .concat()
    }

    fn openai_text_turn(text: &str, finish_reason: &str) -> String {
        let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
            format!(
                "data: {}\n\n",
                serde_json::json!({"id":"c","object":"chat.completion.chunk","created":0,"model":"test/model","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
            )
        };
        [
            chunk(
                serde_json::json!({"role":"assistant","content":text}),
                serde_json::Value::Null,
            ),
            chunk(serde_json::json!({}), serde_json::json!(finish_reason)),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat()
    }

    async fn run_headless<M>(agent: &rig::agent::Agent<M>) -> String
    where
        M: rig::completion::CompletionModel + 'static,
        M::StreamingResponse: Send,
    {
        #[cfg(feature = "hooks")]
        let _dispatcher_guard = crate::tests::fake_model::dispatcher_guard::acquire();
        let (response, _usage, _) = crate::agent::runner::run_print(
            agent,
            "write it",
            false,
            &crate::retry::RetryConfig {
                max_attempts: 1,
                ..crate::retry::RetryConfig::default()
            },
            None,
            Vec::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .expect("truncation must continue, not fail");
        response
    }

    #[tokio::test]
    async fn anthropic_max_tokens_stop_continues_the_turn_with_an_instruction() {
        use rig::client::CompletionClient;
        let (url, server) = sse_server(vec![
            anthropic_text_turn("partial ", "max_tokens"),
            anthropic_text_turn("rest.", "end_turn"),
        ])
        .await;
        let client = rig::providers::anthropic::Client::builder()
            .api_key("unused-test-key")
            .base_url(url)
            .http_client(crate::provider_http::ProviderHttpClient::default())
            .build()
            .unwrap();
        let agent = client
            .agent("claude-test")
            .max_tokens(64)
            .default_max_turns(5)
            .build();
        let response = run_headless(&agent).await;
        let requests = server.await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "truncation must trigger one continuation"
        );
        assert!(
            requests[1].contains("[output truncated]"),
            "{}",
            requests[1]
        );
        assert!(!requests[0].contains("[output truncated]"));
        assert_eq!(response, "partial rest.");
    }

    #[tokio::test]
    async fn openai_length_finish_continues_the_turn_with_an_instruction() {
        use rig::client::CompletionClient;
        let (url, server) = sse_server(vec![
            openai_text_turn("partial ", "length"),
            openai_text_turn("rest.", "stop"),
        ])
        .await;
        let client = rig::providers::openrouter::Client::builder()
            .api_key("unused-test-key")
            .base_url(url)
            .http_client(crate::provider_http::ProviderHttpClient::default())
            .build()
            .unwrap();
        let agent = client
            .agent("test/model")
            .max_tokens(64)
            .default_max_turns(5)
            .build();
        let response = run_headless(&agent).await;
        let requests = server.await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "truncation must trigger one continuation"
        );
        assert!(
            requests[1].contains("[output truncated]"),
            "{}",
            requests[1]
        );
        assert_eq!(response, "partial rest.");
    }

    #[tokio::test]
    async fn complete_responses_do_not_continue() {
        use rig::client::CompletionClient;
        let (url, server) = sse_server(vec![anthropic_text_turn("all done", "end_turn")]).await;
        let client = rig::providers::anthropic::Client::builder()
            .api_key("unused-test-key")
            .base_url(url)
            .http_client(crate::provider_http::ProviderHttpClient::default())
            .build()
            .unwrap();
        let agent = client
            .agent("claude-test")
            .max_tokens(64)
            .default_max_turns(5)
            .build();
        assert_eq!(run_headless(&agent).await, "all done");
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn anthropic_cut_off_tool_arguments_are_reported_not_executed() {
        use futures::StreamExt;
        use rig::client::CompletionClient;
        use rig::completion::CompletionModel;
        let event =
            |kind: &str, data: serde_json::Value| format!("event: {kind}\ndata: {data}\n\n");
        let body = [
            event(
                "message_start",
                serde_json::json!({"type":"message_start","message":{"id":"msg","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":1}}}),
            ),
            event(
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"write","input":{}}}),
            ),
            event(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.txt\",\"content\":\"abc"}}),
            ),
            event(
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":0}),
            ),
            event(
                "message_delta",
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":64}}),
            ),
            event("message_stop", serde_json::json!({"type":"message_stop"})),
        ]
        .concat();
        let (url, server) = sse_server(vec![body]).await;
        let client = rig::providers::anthropic::Client::builder()
            .api_key("unused-test-key")
            .base_url(url)
            .http_client(crate::provider_http::ProviderHttpClient::default())
            .build()
            .unwrap();
        let model = client.completion_model("claude-test");
        let request = model.completion_request("write it").max_tokens(64).build();
        let mut stream = model.stream(request).await.unwrap();
        let mut error = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(rig::streaming::StreamedAssistantContent::ToolCall { .. }) => {
                    panic!("a cut-off tool call must never be emitted")
                }
                Ok(_) => {}
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
            }
        }
        let error = error.expect("cut-off tool arguments must fail the stream");
        assert!(is_output_truncation_error(&error), "{error}");
        let mut state = OutputTruncation::default();
        let decision = state.stream_error(&error).unwrap();
        assert!(
            decision.notice().contains("`write`"),
            "{}",
            decision.notice()
        );
        server.await.unwrap();
    }
}
