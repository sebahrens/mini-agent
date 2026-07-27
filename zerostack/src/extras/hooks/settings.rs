use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize)]
pub(crate) struct HookHandler {
    #[serde(rename = "type")]
    pub kind: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub timeout: Option<u64>,
    #[serde(rename = "async", default)]
    pub is_async: bool,
    #[serde(rename = "if")]
    pub condition: Option<String>,
    #[serde(default)]
    pub once: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct HookGroup {
    pub matcher: Option<String>,
    pub hooks: Vec<HookHandler>,
}

pub(crate) type HooksConfig = HashMap<String, Vec<HookGroup>>;

/// Parses the JSON value found at the top-level `hooks` key of settings.json.
pub(crate) fn parse_hooks_config(value: &serde_json::Value) -> Result<HooksConfig, String> {
    let config: HooksConfig =
        serde_json::from_value(value.clone()).map_err(|e| format!("invalid hooks config: {e}"))?;

    for (event, groups) in &config {
        for group in groups {
            for handler in &group.hooks {
                if handler.kind != "command" {
                    return Err(format!(
                        "unsupported hook type {:?} for event {event:?} (only \"command\" is supported in v1)",
                        handler.kind
                    ));
                }
            }
        }
    }

    Ok(config)
}
