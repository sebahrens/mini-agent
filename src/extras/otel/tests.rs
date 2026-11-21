use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(base: &str) -> OtelConfig {
    OtelConfig::from_lookup(|name| match name {
        "OTEL_EXPORTER_OTLP_ENDPOINT" => Some(base.to_string()),
        "OTEL_EXPORTER_OTLP_HEADERS" => Some("x-team=core, authorization=Bearer t".to_string()),
        _ => None,
    })
    .unwrap()
}

fn usage() -> UsageDelta {
    UsageDelta {
        input_tokens: 100,
        output_tokens: 20,
        cached_input_tokens: 5,
        reported_cost_nano_usd: 2_500_000,
        ..Default::default()
    }
}

#[test]
fn unconfigured_environment_builds_no_collector() {
    assert!(OtelConfig::from_lookup(|_| None).is_none());
    // The kill switch wins over an endpoint.
    assert!(
        OtelConfig::from_lookup(|name| match name {
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some("http://127.0.0.1:1".into()),
            "OTEL_SDK_DISABLED" => Some("true".into()),
            _ => None,
        })
        .is_none()
    );
}

#[test]
fn endpoint_variables_resolve_signal_urls_and_headers() {
    let cfg = config("http://collector:4318/");
    assert_eq!(cfg.metrics_url, "http://collector:4318/v1/metrics");
    assert_eq!(cfg.traces_url, "http://collector:4318/v1/traces");
    assert_eq!(cfg.headers[0], ("x-team".to_string(), "core".to_string()));
    assert_eq!(cfg.headers[1].0, "authorization");
    let specific = OtelConfig::from_lookup(|name| {
        (name == "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").then(|| "http://t/v1/traces".to_string())
    })
    .unwrap();
    assert!(specific.metrics_url.is_empty());
    assert_eq!(specific.traces_url, "http://t/v1/traces");
}

#[test]
fn one_turn_with_a_tool_call_yields_expected_metrics_and_a_span_tree() {
    let collector = Collector::new(config("http://127.0.0.1:1"));
    collector.tool_call("c1", "shell");
    collector.tool_result("c1", false);
    collector.tool_call("c2", "edit");
    collector.tool_result("c2", true);
    collector.usage(&usage());
    collector.end_turn(false);

    let metrics = collector.metrics_payload().to_string();
    for name in [
        "mini_agent.session.count",
        "mini_agent.token.usage",
        "mini_agent.cost.usage",
        "mini_agent.tool.calls",
    ] {
        assert!(metrics.contains(name), "{name} missing: {metrics}");
    }
    assert!(metrics.contains("\"stringValue\":\"cache_read\""));
    assert!(metrics.contains("\"stringValue\":\"error\""));

    let traces = collector.take_traces_payload().unwrap();
    let spans = traces["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap();
    assert_eq!(spans.len(), 3);
    let turn = spans
        .iter()
        .find(|s| s["name"] == "mini_agent.turn")
        .unwrap();
    assert!(turn.get("parentSpanId").is_none());
    let children: Vec<_> = spans
        .iter()
        .filter(|s| s["name"] == "mini_agent.tool_call")
        .collect();
    assert_eq!(children.len(), 2);
    for child in children {
        assert_eq!(child["parentSpanId"], turn["spanId"]);
        assert_eq!(child["traceId"], turn["traceId"]);
    }
    assert!(
        collector.take_traces_payload().is_none(),
        "spans are taken once"
    );
}

#[test]
fn payloads_never_carry_prompt_or_tool_argument_content() {
    let collector = Collector::new(config("http://127.0.0.1:1"));
    collector.observe(&AgentEvent::ToolCall {
        id: "c1".into(),
        name: "shell".into(),
        args: json!({"command": "cat /secret/SENTINEL_ARG"}),
    });
    collector.observe(&AgentEvent::ToolResult {
        id: "c1".into(),
        name: "shell".into(),
        output: "SENTINEL_OUTPUT".into(),
        images: Vec::new(),
        is_error: false,
    });
    collector.observe(&AgentEvent::Done {
        response: "SENTINEL_RESPONSE".into(),
        interactions: Vec::new(),
    });
    let all = format!(
        "{}{}",
        collector.metrics_payload(),
        collector.take_traces_payload().unwrap()
    );
    assert!(!all.contains("SENTINEL"), "{all}");
}

async fn stub() -> (String, tokio::task::JoinHandle<Vec<(String, String)>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let mut len = 0;
            loop {
                let n = socket.read(&mut buf[len..]).await.unwrap();
                len += n;
                let text = String::from_utf8_lossy(&buf[..len]).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let want = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= want || n == 0 {
                        seen.push((head.lines().next().unwrap().to_string(), body.to_string()));
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
        }
        seen
    });
    (format!("http://{addr}"), task)
}

#[tokio::test]
async fn flush_posts_metrics_and_traces_to_a_local_otlp_stub() {
    let (base, stub) = stub().await;
    let collector = Collector::new(config(&base));
    collector.observe(&AgentEvent::ToolCall {
        id: "c1".into(),
        name: "shell".into(),
        args: json!({}),
    });
    collector.observe(&AgentEvent::ToolResult {
        id: "c1".into(),
        name: "shell".into(),
        output: "ok".into(),
        images: Vec::new(),
        is_error: false,
    });
    collector.observe(&AgentEvent::Done {
        response: "done".into(),
        interactions: Vec::new(),
    });
    collector.flush().await;
    let seen = stub.await.unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[0].0.starts_with("POST /v1/metrics "),
        "{:?}",
        seen[0].0
    );
    assert!(seen[0].1.contains("mini_agent.tool.calls"));
    assert!(seen[1].0.starts_with("POST /v1/traces "), "{:?}", seen[1].0);
    assert!(seen[1].1.contains("mini_agent.turn"));
}

#[tokio::test]
async fn an_unreachable_collector_never_fails_the_caller() {
    let collector = Collector::new(config("http://127.0.0.1:9"));
    collector.tool_call("c", "shell");
    collector.tool_result("c", false);
    collector.end_turn(false);
    collector.flush().await;
}
