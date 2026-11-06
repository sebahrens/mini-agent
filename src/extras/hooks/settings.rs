use std::collections::{BTreeMap, HashMap};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum HookTrust {
    /// Require the configured general workspace sandbox. Missing or invalid
    /// containment denies the launch.
    #[default]
    Sandboxed,
    /// Explicit configuration-author consent to run without OS containment.
    /// The hook still receives a cleared, minimal environment and project cwd.
    Trusted,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
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
    #[serde(default)]
    pub trust: HookTrust,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct HookGroup {
    pub matcher: Option<String>,
    pub hooks: Vec<HookHandler>,
}

pub(crate) type HooksConfig = HashMap<String, Vec<HookGroup>>;

pub(crate) const SUPPORTED_HOOK_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
    "StopFailure",
    "SessionStart",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "Notification",
    "PermissionRequest",
    "InstructionsLoaded",
    "FileChanged",
    "CwdChanged",
];

/// Claude Code hook events this build accepts in settings but never
/// dispatches (listed by `/hooks` so ported configurations are not silently
/// assumed to fire).
pub(crate) const KNOWN_UNDISPATCHED_HOOK_EVENTS: &[&str] = &[
    "PermissionDenied",
    "ConfigChange",
    "TaskCreated",
    "TaskCompleted",
    "WorktreeCreate",
    "WorktreeRemove",
    "Elicitation",
    "ElicitationResult",
    "PostToolBatch",
];

/// Handler `type` values this build runs. `http` requires the
/// `hooks_http_enabled` opt-in, `mcp_tool` the `hooks_mcp_tool_enabled` opt-in
/// and `prompt`/`agent` the `hooks_model_enabled` opt-in; unknown types are
/// skipped per handler with a warning.
pub(crate) const SUPPORTED_HOOK_TYPES: &[&str] =
    &["command", "http", "mcp_tool", "prompt", "agent"];

const MAX_MODEL_HOOK_PROMPT_BYTES: usize = 32 * 1024;
const MAX_MCP_HOOK_INPUT_BYTES: usize = 64 * 1024;

fn reject_subprocess_fields(
    raw: &serde_json::Map<String, serde_json::Value>,
    kind: &str,
) -> Result<(), String> {
    for field in ["command", "args", "env", "if", "url", "headers"] {
        if raw.get(field).is_some_and(|value| !value.is_null()) {
            return Err(format!("{kind} hook does not accept `{field}`"));
        }
    }
    Ok(())
}

/// Validates a `prompt` or `agent` handler. The prompt text is stored in
/// `command` and the optional `model` in `args`, so the existing trust
/// identity, approval description and dedupe keys cover both.
fn model_binding(
    raw: Option<&serde_json::Value>,
    kind: &str,
) -> Result<(String, Option<Vec<String>>), String> {
    let raw = raw
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| format!("{kind} hook must be an object"))?;
    reject_subprocess_fields(raw, kind)?;
    let prompt = raw
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or_else(|| format!("{kind} hook requires a non-empty string `prompt`"))?;
    if prompt.len() > MAX_MODEL_HOOK_PROMPT_BYTES || prompt.contains('\0') {
        return Err(format!("{kind} hook prompt is too long or invalid"));
    }
    let model = match raw.get("model") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            let model = value
                .as_str()
                .filter(|model| {
                    !model.is_empty() && model.len() <= 256 && !model.chars().any(char::is_control)
                })
                .ok_or_else(|| format!("{kind} hook `model` must be a short string"))?;
            Some(vec![model.to_owned()])
        }
    };
    Ok((prompt.to_owned(), model))
}

/// Validates an `mcp_tool` handler: `server` goes to `command`, and `tool`
/// plus the canonical JSON `input` object to `args`.
fn mcp_tool_binding(raw: Option<&serde_json::Value>) -> Result<(String, Vec<String>), String> {
    let raw = raw
        .and_then(serde_json::Value::as_object)
        .ok_or("mcp_tool hook must be an object")?;
    reject_subprocess_fields(raw, "mcp_tool")?;
    let name = |field: &str| {
        raw.get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|value| {
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
            })
            .map(str::to_owned)
            .ok_or_else(|| format!("mcp_tool hook requires a short string `{field}`"))
    };
    let (server, tool) = (name("server")?, name("tool")?);
    let input = match raw.get("input") {
        None | Some(serde_json::Value::Null) => serde_json::json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return Err("mcp_tool hook `input` must be an object".into()),
    };
    let input = serde_json::to_string(&input).map_err(|_| "mcp_tool hook input is invalid")?;
    if input.len() > MAX_MCP_HOOK_INPUT_BYTES {
        return Err("mcp_tool hook input is too large".into());
    }
    Ok((server, vec![tool, input]))
}

const MAX_HTTP_HOOK_HEADERS: usize = 32;
const MAX_HTTP_HOOK_HEADER_BYTES: usize = 8 * 1024;
const MAX_HTTP_HOOK_URL_BYTES: usize = 8 * 1024;

/// Validates an `http` handler's raw JSON. The URL is stored in `command` and
/// the literal request headers in `env`, so the existing trust identity,
/// approval description and dedupe keys cover both without a schema change.
fn http_binding(
    raw: Option<&serde_json::Value>,
) -> Result<(String, BTreeMap<String, String>), String> {
    let raw = raw
        .and_then(serde_json::Value::as_object)
        .ok_or("http hook must be an object")?;
    for field in ["command", "args", "env", "if"] {
        if raw.get(field).is_some_and(|value| !value.is_null()) {
            return Err(format!("http hook does not accept `{field}`"));
        }
    }
    let url = raw
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or("http hook requires a string `url`")?;
    if url.len() > MAX_HTTP_HOOK_URL_BYTES {
        return Err("http hook url is too long".into());
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| "http hook url is invalid")?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("http hook url must be an absolute http(s) URL".into());
    }
    let mut headers = BTreeMap::new();
    if let Some(raw_headers) = raw.get("headers") {
        let map = raw_headers
            .as_object()
            .ok_or("http hook `headers` must be an object of strings")?;
        if map.len() > MAX_HTTP_HOOK_HEADERS {
            return Err("http hook has too many headers".into());
        }
        for (name, value) in map {
            let value = value
                .as_str()
                .ok_or("http hook header values must be strings")?;
            if value.len() > MAX_HTTP_HOOK_HEADER_BYTES
                || reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err()
                || reqwest::header::HeaderValue::from_str(value).is_err()
            {
                return Err("http hook header is invalid".into());
            }
            headers.insert(name.clone(), value.to_owned());
        }
    }
    Ok((url.to_owned(), headers))
}

/// Parses the JSON value found at the top-level `hooks` key of settings.json.
pub(crate) fn parse_hooks_config(value: &serde_json::Value) -> Result<HooksConfig, String> {
    let mut config: HooksConfig =
        serde_json::from_value(value.clone()).map_err(|e| format!("invalid hooks config: {e}"))?;

    for (event, groups) in &mut config {
        if !SUPPORTED_HOOK_EVENTS.contains(&event.as_str()) {
            // A spelling that differs from a known event only by case is
            // almost certainly a typo (e.g. `PreToolUSe`), so it still fails
            // the whole file with a corrective message. Anything else is a
            // genuinely unrecognized or future event name (`FutureEvent`,
            // a newer Claude Code event this build has no dispatcher for,
            // ...): tolerate it so one unrecognized event does not discard
            // every hook — including sibling events this build does
            // understand — in the same settings file.
            if SUPPORTED_HOOK_EVENTS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(event))
            {
                return Err(format!(
                    "unsupported hook event {event:?}; expected one of {}",
                    SUPPORTED_HOOK_EVENTS.join(", ")
                ));
            }
            tracing::warn!(
                "hooks: unsupported hook event {event:?} is not dispatched by this build; expected one of {} (keeping it loaded, inert)",
                SUPPORTED_HOOK_EVENTS.join(", ")
            );
        }
        let raw_groups = value
            .get(event.as_str())
            .and_then(serde_json::Value::as_array);
        for (group_index, group) in groups.iter_mut().enumerate() {
            let raw_hooks = raw_groups
                .and_then(|groups| groups.get(group_index))
                .and_then(|group| group.get("hooks"))
                .and_then(serde_json::Value::as_array);
            let mut kept = Vec::with_capacity(group.hooks.len());
            for (index, mut handler) in std::mem::take(&mut group.hooks).into_iter().enumerate() {
                match handler.kind.as_str() {
                    "command" => {
                        if handler.args.is_none()
                            && let Some(command) = handler.command.as_deref()
                        {
                            let mut argv = split_command_string(command)?;
                            // Preserve existing single-program bindings
                            // (including Windows backslashes) and their
                            // persisted trust identity.
                            if argv.len() != 1
                                || command
                                    .chars()
                                    .any(|ch| ch.is_whitespace() || matches!(ch, '\'' | '"'))
                            {
                                handler.command = Some(argv.remove(0));
                                handler.args = (!argv.is_empty()).then_some(argv);
                            }
                        }
                        kept.push(handler);
                    }
                    "http" => match http_binding(raw_hooks.and_then(|hooks| hooks.get(index))) {
                        Ok((url, headers)) => {
                            handler.command = Some(url);
                            handler.args = None;
                            handler.env = headers;
                            kept.push(handler);
                        }
                        Err(error) => tracing::warn!(
                            "hooks: skipping one http hook for event {event:?}: {error}"
                        ),
                    },
                    "mcp_tool" => {
                        match mcp_tool_binding(raw_hooks.and_then(|hooks| hooks.get(index))) {
                            Ok((server, args)) => {
                                handler.command = Some(server);
                                handler.args = Some(args);
                                handler.condition = None;
                                handler.env.clear();
                                kept.push(handler);
                            }
                            Err(error) => tracing::warn!(
                                "hooks: skipping one mcp_tool hook for event {event:?}: {error}"
                            ),
                        }
                    }
                    kind @ ("prompt" | "agent") => {
                        match model_binding(raw_hooks.and_then(|hooks| hooks.get(index)), kind) {
                            Ok((prompt, args)) => {
                                handler.command = Some(prompt);
                                handler.args = args;
                                handler.condition = None;
                                handler.env.clear();
                                kept.push(handler);
                            }
                            Err(error) => tracing::warn!(
                                "hooks: skipping one {kind} hook for event {event:?}: {error}"
                            ),
                        }
                    }
                    other => tracing::warn!(
                        "hooks: skipping one hook of unsupported type {other:?} for event {event:?}; this build runs {} (other hooks in the file stay loaded)",
                        SUPPORTED_HOOK_TYPES.join(", ")
                    ),
                }
            }
            group.hooks = kept;
        }
    }

    Ok(config)
}

/// Command-string compatibility is quoting/tokenization only. The existing
/// direct-exec policy, content pins and explicit environment remain authoritative.
/// Operators who need shell syntax name a shell and its command flag explicitly.
fn split_command_string(command: &str) -> Result<Vec<String>, String> {
    const ERROR: &str =
        "invalid hook command string (values omitted; use explicit command and args)";
    if command.len() > 32 * 1024 || command.contains('\0') {
        return Err(ERROR.into());
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = None;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        match (quote, character) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), character) => {
                word.push(character);
            }
            (Some('"'), '\\') => {
                let next = chars.next().ok_or(ERROR)?;
                if !matches!(next, '\\' | '"' | '$' | '`' | '\n') {
                    word.push('\\');
                }
                if next != '\n' {
                    word.push(next);
                }
            }
            (Some('"'), character) => {
                word.push(character);
            }
            (None, '\'' | '"') => {
                quote = Some(character);
                started = true;
            }
            (None, '\\') => {
                let next = chars.next().ok_or(ERROR)?;
                if next != '\n' {
                    word.push(next);
                    started = true;
                }
            }
            (None, character) if character.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                    if words.len() > 128 {
                        return Err(ERROR.into());
                    }
                }
            }
            (None, ';' | '|' | '&' | '<' | '>') => return Err(ERROR.into()),
            (None, character) => {
                word.push(character);
                started = true;
            }
            _ => unreachable!(),
        }
    }
    if quote.is_some() {
        return Err(ERROR.into());
    }
    if started {
        words.push(word);
    }
    if words.len() > 128 || words.first().is_none_or(|program| program.is_empty()) {
        return Err(ERROR.into());
    }
    Ok(words)
}

#[cfg(test)]
mod command60_tests {
    use super::*;
    #[test]
    fn hooks60_command_strings_tokenize_quotes_without_shell_expansion() {
        let config = parse_hooks_config(&serde_json::json!({"SessionStart":[{"hooks":[{
            "type":"command", "command":"'/path with spaces/guard' \"two words\" '' '$SECRET'"
        }]}]}))
        .unwrap();
        let handler = &config["SessionStart"][0].hooks[0];
        assert_eq!(handler.command.as_deref(), Some("/path with spaces/guard"));
        assert_eq!(
            handler.args.as_deref().unwrap(),
            ["two words", "", "$SECRET"]
        );
        let config = parse_hooks_config(&serde_json::json!({"SessionStart":[{"hooks":[{
            "type":"command", "command":"/path with spaces/guard", "args":[]
        }]}]}))
        .unwrap();
        assert_eq!(
            config["SessionStart"][0].hooks[0].command.as_deref(),
            Some("/path with spaces/guard")
        );
        for program in ["true", r"C:\Windows\guard.exe"] {
            let config = parse_hooks_config(&serde_json::json!({"SessionStart":[{"hooks":[{
                "type":"command", "command":program
            }]}]}))
            .unwrap();
            let handler = &config["SessionStart"][0].hooks[0];
            assert_eq!(handler.command.as_deref(), Some(program));
            assert!(
                handler.args.is_none(),
                "old single-program trust identity stays unchanged"
            );
        }
        for command in [
            "",
            "''",
            "echo 'private-unclosed",
            "echo private; bad",
            "echo private | bad",
        ] {
            let error = split_command_string(command).unwrap_err();
            assert!(!error.contains("private"));
        }
        assert!(split_command_string(&"x".repeat(32 * 1024 + 1)).is_err());
    }
}
