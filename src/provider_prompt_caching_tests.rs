//! Claude-family prompt-caching opt-in on OpenAI-compatible custom providers
//! (mini-agent-u9jj3): real-wire proof that a gateway sees `cache_control`
//! only when a `custom_providers` entry opts in *and* the requested model id
//! is in the Claude family, using the same loopback `Wire` harness as the
//! sibling native-cache tests in `provider_cache_tests.rs`.

use super::*;

#[test]
fn is_claude_model_family_matches_prefixes_and_segments_not_bare_substrings() {
    for model_id in [
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "bedrock.anthropic.claude-sonnet-5-5",
        "vertex.anthropic.claude-opus-5-5",
        "~bedrock.anthropic.claude-sonnet-5-5",
        "anthropic/claude-3.5-sonnet",
        "openrouter/anthropic/claude-3.5-sonnet",
    ] {
        assert!(
            is_claude_model_family(model_id),
            "{model_id} should match the Claude family"
        );
    }
    for model_id in [
        "gpt-5.5",
        "declaude-mini",
        "not-claude-at-all",
        "my-claude-adjacent-model",
        "bedrock.amazon.titan-text",
        "glm-5.3",
    ] {
        assert!(
            !is_claude_model_family(model_id),
            "{model_id} should not match the Claude family"
        );
    }
}

fn openai_wire_client(wire: &Wire, prompt_caching: bool) -> AnyClient {
    let config: Config = serde_json::from_value(json!({
        "custom_providers": {
            "cache-probe": {
                "provider_type": "openai",
                "base_url": wire.base,
                "prompt_caching": prompt_caching,
            }
        }
    }))
    .unwrap();
    create_client(
        "cache-probe",
        Some("cache-fake-key"),
        config.custom_providers_map(),
        None,
    )
    .unwrap()
}

fn minimal_chat_completion_response() -> String {
    json!({
        "id": "fixture",
        "object": "chat.completion",
        "created": 0,
        "model": "fixture",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop",
            "logprobs": null
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
    .to_string()
}

/// Builds a model for `model_id` from `client` and runs one completion
/// against the wire, returning the captured requests after the exchange.
async fn run_one_completion(client: &AnyClient, model_id: &str, wire: Wire) -> Vec<Value> {
    let AnyModel::OpenAI(model) =
        client.completion_model_with_cache_ttl(model_id, Default::default())
    else {
        unreachable!("cache-probe is configured as an openai-type custom provider")
    };
    let OpenAiModel::Completions(model) = model else {
        panic!("a custom openai provider with a base_url defaults to the Completions API")
    };
    let request = model
        .completion_request("new turn")
        .preamble("system prompt".to_string())
        .build();
    model.completion(request).await.unwrap();
    wire.finish()
}

#[tokio::test]
async fn prompt_caching_opt_in_marks_claude_model_request() {
    let wire = Wire::at_path(
        vec![(false, minimal_chat_completion_response())],
        "/chat/completions",
    );
    let client = openai_wire_client(&wire, true);
    let requests = run_one_completion(&client, "bedrock.anthropic.claude-sonnet-5-5", wire).await;
    assert_eq!(requests.len(), 1);
    let body = &requests[0];
    let messages = body["messages"].as_array().expect("messages array");
    let system = messages
        .iter()
        .find(|m| m["role"] == "system")
        .expect("system message present");
    assert!(
        serde_json::to_string(&system["content"])
            .unwrap()
            .contains("cache_control"),
        "system message should carry a cache_control breakpoint when opted in: {body}"
    );
}

#[tokio::test]
async fn prompt_caching_opt_in_is_ignored_for_non_claude_model() {
    let wire = Wire::at_path(
        vec![(false, minimal_chat_completion_response())],
        "/chat/completions",
    );
    let client = openai_wire_client(&wire, true);
    let requests = run_one_completion(&client, "gpt-5.5", wire).await;
    let body_text = serde_json::to_string(&requests[0]).unwrap();
    assert!(
        !body_text.contains("cache_control"),
        "a non-Claude model id must not get cache_control markers even when the provider opted in: {body_text}"
    );
}

#[tokio::test]
async fn prompt_caching_off_by_default_leaves_claude_request_unmarked() {
    let wire = Wire::at_path(
        vec![(false, minimal_chat_completion_response())],
        "/chat/completions",
    );
    // No `prompt_caching` key at all: the default is off.
    let client = openai_wire_client(&wire, false);
    let requests = run_one_completion(&client, "bedrock.anthropic.claude-sonnet-5-5", wire).await;
    let body_text = serde_json::to_string(&requests[0]).unwrap();
    assert!(
        !body_text.contains("cache_control"),
        "the opt-in defaults to off, so a gateway that rejects cache_control stays unaffected: {body_text}"
    );
}

#[tokio::test]
async fn prompt_caching_1h_ttl_reaches_the_wire_request() {
    let wire = Wire::at_path(
        vec![(false, minimal_chat_completion_response())],
        "/chat/completions",
    );
    let config: Config = serde_json::from_value(json!({
        "cache_ttl": "1h",
        "custom_providers": {
            "cache-probe": {
                "provider_type": "openai",
                "base_url": wire.base,
                "prompt_caching": true,
            }
        }
    }))
    .unwrap();
    let client = create_client(
        "cache-probe",
        Some("cache-fake-key"),
        config.custom_providers_map(),
        None,
    )
    .unwrap();
    let AnyModel::OpenAI(OpenAiModel::Completions(model)) = client.completion_model_with_cache_ttl(
        "bedrock.anthropic.claude-sonnet-5-5",
        config.cache_ttl.unwrap_or_default(),
    ) else {
        unreachable!()
    };
    let request = model
        .completion_request("new turn")
        .preamble("system prompt".to_string())
        .build();
    model.completion(request).await.unwrap();
    let requests = wire.finish();
    let body = &requests[0];
    let messages = body["messages"].as_array().expect("messages array");
    let system = messages
        .iter()
        .find(|m| m["role"] == "system")
        .expect("system message present");
    assert!(
        serde_json::to_string(&system["content"])
            .unwrap()
            .contains(r#""ttl":"1h""#),
        "the owner's 1h cache_ttl setting should reach the wire marker: {body}"
    );
}
