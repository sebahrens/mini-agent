//! Native cache usage capture before any TTL/rate changes.

use super::*;

#[tokio::test]
async fn cache71_native_start_cache_usage_survives_output_only_delta() {
    let response = event(
        "message_start",
        json!({
            "type":"message_start", "message":{
                "id":"msg_cache_probe", "type":"message", "role":"assistant",
                "model":"claude-opus-5-5", "content":[], "stop_reason":null,
                "stop_sequence":null,"usage":{
                    "input_tokens":10,"output_tokens":0,
                    "cache_read_input_tokens":120,"cache_creation_input_tokens":80,
                    "cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50}
                }
            }
        }),
    ) + &text(0, "done")
        + &finish();
    let wire = Wire::new(vec![(true, response)]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let mut stream = model
        .completion_request("cache usage capture")
        .max_tokens(1000)
        .stream()
        .await
        .unwrap();
    while let Some(item) = stream.next().await {
        item.unwrap();
    }
    let usage = stream.usage();
    let raw = serde_json::to_value(stream.response.as_ref().unwrap()).unwrap();
    println!(
        "CACHE71_USAGE_CAPTURE={}",
        json!({"generic_usage":usage,"raw_native_response":raw})
    );
    assert_eq!(wire.finish().len(), 1);
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(
        usage.cached_input_tokens, 120,
        "message_start cache hits must not disappear"
    );
    assert_eq!(
        usage.cache_creation_input_tokens, 80,
        "message_start cache writes must not disappear"
    );
    assert_eq!(
        raw["usage"]["cache_creation"]["ephemeral_5m_input_tokens"],
        30
    );
    assert_eq!(
        raw["usage"]["cache_creation"]["ephemeral_1h_input_tokens"],
        50
    );
}

fn cached_start(usage: Value) -> String {
    event(
        "message_start",
        json!({"type":"message_start","message":{
            "id":"msg_cache_delta","type":"message","role":"assistant","model":"claude-opus-5-5",
            "content":[],"stop_reason":null,"stop_sequence":null,"usage":usage
        }}),
    )
}

fn cached_delta(stop: bool, usage: Value) -> String {
    event(
        "message_delta",
        json!({"type":"message_delta","delta":{
        "stop_reason":if stop {Some("end_turn")} else {None},"stop_sequence":null
    },"usage":usage}),
    )
}

#[tokio::test]
async fn cache71_native_cumulative_delta_overwrites_and_does_not_infer_missing_split() {
    let initial = json!({"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":120,
        "cache_creation_input_tokens":80,"cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50}});
    for (delta, expected_read, expected_write, expected_split) in [
        (
            json!({"input_tokens":20,"output_tokens":2,"cache_read_input_tokens":140,
            "cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":40,"ephemeral_1h_input_tokens":60}}),
            140,
            100,
            Some(json!({"ephemeral_5m_input_tokens":40,"ephemeral_1h_input_tokens":60})),
        ),
        (
            json!({"input_tokens":20,"output_tokens":2,"cache_read_input_tokens":0,
            "cache_creation_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}),
            0,
            0,
            Some(json!({"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0})),
        ),
        (
            json!({"input_tokens":20,"output_tokens":2,"cache_creation_input_tokens":100}),
            130,
            100,
            None,
        ),
    ] {
        let body = cached_start(initial.clone())
            + &text(0, "done")
            + &cached_delta(
                false,
                json!({"output_tokens":1,"cache_read_input_tokens":130}),
            )
            + &cached_delta(true, delta)
            + &event("message_stop", json!({"type":"message_stop"}));
        let wire = Wire::new(vec![(true, body)]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let mut response = model
            .completion_request("cumulative usage")
            .max_tokens(1000)
            .stream()
            .await
            .unwrap();
        while let Some(item) = response.next().await {
            item.unwrap();
        }
        let usage = response.usage();
        let raw = serde_json::to_value(response.response.as_ref().unwrap()).unwrap();
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cached_input_tokens, expected_read);
        assert_eq!(usage.cache_creation_input_tokens, expected_write);
        match expected_split {
            Some(split) => assert_eq!(raw["usage"]["cache_creation"], split),
            None => assert!(raw["usage"].get("cache_creation").is_none()),
        }
        assert_eq!(wire.finish().len(), 1);
    }
}

#[tokio::test]
async fn cache71_native_invalid_reported_counts_fail_without_inventing_usage() {
    for (usage, expected) in [
        (
            json!({"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":79,
            "cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50}}),
            "inconsistent Anthropic cache creation usage",
        ),
        (
            json!({"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":u64::MAX,
            "cache_creation":{"ephemeral_5m_input_tokens":u64::MAX,"ephemeral_1h_input_tokens":1}}),
            "Anthropic cache creation usage exceeds limits",
        ),
        (
            json!({"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":u64::MAX}),
            "Anthropic token usage exceeds limits",
        ),
    ] {
        let wire = Wire::new(vec![(
            true,
            cached_start(usage) + &text(0, "unused") + &finish(),
        )]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let mut stream = model
            .completion_request("malformed usage")
            .max_tokens(1000)
            .stream()
            .await
            .unwrap();
        let mut errors = Vec::new();
        while let Some(item) = stream.next().await {
            if let Err(error) = item {
                errors.push(error);
            }
        }
        assert_eq!(errors.len(), 1);
        let rig::completion::CompletionError::ResponseError(error) = errors.pop().unwrap() else {
            panic!("expected static response error")
        };
        assert_eq!(error, expected);
        assert_eq!(stream.usage(), rig::completion::Usage::new());
        assert_eq!(wire.finish().len(), 1);
    }
}

#[tokio::test]
async fn cache71_native_blocking_preserves_reported_split_and_rejects_inconsistent_total() {
    for total in [80, 79] {
        let response = json!({"id":"msg_cache_blocking","type":"message","role":"assistant",
            "content":[{"type":"text","text":"done"}],"model":"claude-opus-5-5",
            "stop_reason":"end_turn","stop_sequence":null,
            "usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":120,
                "cache_creation_input_tokens":total,"cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50}}});
        let wire = Wire::new(vec![(false, response.to_string())]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let result = model
            .completion(
                model
                    .completion_request("blocking cache")
                    .max_tokens(1000)
                    .build(),
            )
            .await;
        if total == 80 {
            let response = result.unwrap();
            assert_eq!(response.usage.cached_input_tokens, 120);
            assert_eq!(response.usage.cache_creation_input_tokens, 80);
            let raw = serde_json::to_value(response.raw_response).unwrap();
            assert_eq!(
                raw["usage"]["cache_creation"],
                json!({"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50})
            );
        } else {
            let rig::completion::CompletionError::ResponseError(error) = result.err().unwrap()
            else {
                panic!("expected response error")
            };
            assert_eq!(error, "inconsistent Anthropic cache creation usage");
        }
        assert_eq!(wire.finish().len(), 1);
    }
}
