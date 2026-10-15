//! The documented Sign in with ChatGPT Responses policy, isolated from API keys.
use anyhow::ensure;
use serde_json::{Value, json};

pub(crate) const MAX_BODY: usize = 16 * 1024 * 1024;
pub(crate) fn endpoint(method: &http::Method, uri: &http::Uri) -> anyhow::Result<()> {
    ensure!(
        uri.scheme_str() == Some("https")
            && uri
                .authority()
                .is_some_and(|a| a.as_str() == "api.openai.com")
            && uri.query().is_none(),
        "ChatGPT credentials are restricted to the official OpenAI endpoint"
    );
    ensure!(
        (method == http::Method::GET && uri.path() == "/v1/models")
            || (method == http::Method::POST && uri.path() == "/v1/responses"),
        "This endpoint is not supported by ChatGPT plan authentication"
    );
    Ok(())
}
pub(crate) fn body(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    ensure!(
        bytes.len() <= MAX_BODY,
        "ChatGPT request byte limit exceeded"
    );
    let mut value: Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("Invalid ChatGPT Responses request"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Invalid ChatGPT Responses request"))?;
    // Rig constructs these from ordinary global generation settings. This
    // subscription route does not support them; it never forwards them.
    object.remove("max_output_tokens");
    object.remove("temperature");
    for name in [
        "background",
        "conversation",
        "max_tool_calls",
        "metadata",
        "moderation",
        "multi_agent",
        "prompt",
        "prompt_cache_retention",
        "safety_identifier",
        "top_logprobs",
        "top_p",
        "truncation",
        "user",
        "previous_response_id",
    ] {
        ensure!(
            object.get(name).is_none_or(Value::is_null),
            "ChatGPT plan authentication does not support request field {name}"
        );
        object.remove(name);
    }
    object.insert("store".into(), json!(false));
    object.insert("stream".into(), json!(true));
    let tools = object.remove("tools").unwrap_or(json!([]));
    let tools = tools
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid ChatGPT tools"))?;
    ensure!(
        tools.iter().all(|t| matches!(
            t.get("type").and_then(Value::as_str),
            Some("function" | "custom")
        )),
        "Hosted tools are not supported by ChatGPT plan authentication"
    );
    let input = object
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("ChatGPT plan requests require full array input"))?;
    for item in input.iter_mut() {
        ensure!(
            item.get("type").and_then(Value::as_str) != Some("item_reference"),
            "Start a new session: this history requires server-stored Responses data"
        );
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            ensure!(
                item.get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty()),
                "Start a new session: saved reasoning lacks stateless encrypted content"
            );
        }
        if item.get("role").and_then(Value::as_str) == Some("system") {
            item["role"] = json!("developer");
        }
    }
    if !tools.is_empty() {
        input.insert(
            0,
            json!({"type":"additional_tools", "role":"developer", "tools":tools}),
        );
    }
    let include = object
        .entry("include")
        .or_insert(json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("Invalid ChatGPT include setting"))?;
    if !include
        .iter()
        .any(|v| v.as_str() == Some("reasoning.encrypted_content"))
    {
        include.push(json!("reasoning.encrypted_content"));
    }
    let result = serde_json::to_vec(&value)?;
    ensure!(
        result.len() <= MAX_BODY,
        "ChatGPT request byte limit exceeded"
    );
    Ok(result)
}

/// Non-streaming SDK helpers consume the provider's actual completed SSE
/// envelope. This neither makes a second request nor estimates usage.
pub(crate) fn completed_sse(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    ensure!(
        bytes.len() <= MAX_BODY,
        "ChatGPT response byte limit exceeded"
    );
    let source =
        std::str::from_utf8(bytes).map_err(|_| anyhow::anyhow!("Invalid ChatGPT event stream"))?;
    let mut completed = None;
    for event in source.replace("\r\n", "\n").split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let value: Value =
            serde_json::from_str(&data).map_err(|_| anyhow::anyhow!("Invalid ChatGPT event"))?;
        match value.get("type").and_then(Value::as_str) {
            Some("response.completed") => {
                ensure!(completed.is_none(), "Duplicate ChatGPT completion");
                let response = value
                    .get("response")
                    .filter(|r| r.get("status").and_then(Value::as_str) == Some("completed"))
                    .ok_or_else(|| anyhow::anyhow!("ChatGPT response did not complete"))?;
                completed = Some(serde_json::to_vec(response)?);
            }
            Some("error" | "response.failed" | "response.incomplete") => {
                anyhow::bail!("ChatGPT response did not complete")
            }
            _ => (),
        }
    }
    completed.ok_or_else(|| anyhow::anyhow!("ChatGPT response ended before completion"))
}

pub(crate) fn models(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("Invalid ChatGPT model catalogue"))?;
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Invalid ChatGPT model catalogue"))?;
    ensure!(
        models.len() <= 10_000,
        "ChatGPT model catalogue limit exceeded"
    );
    let data: Vec<_> = models
        .iter()
        .filter(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
        .map(|m| json!({"id":m.get("slug"), "name":m.get("display_name")}))
        .collect();
    Ok(serde_json::to_vec(&json!({"data":data}))?)
}
