//! Typed main-session generation controls. These values are configuration
//! data; they never restore permission, replay or publication authority.
use crate::config::types::ReasoningEffort;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SessionReasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking: Option<ThinkingMode>,
}
impl SessionReasoning {
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThinkingMode {
    On,
    Off,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protocol {
    Anthropic,
    OpenRouter,
    OpenAiResponses,
    OpenAiCompletions,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Session,
    Raw,
    Config,
    Provider,
}
impl Source {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Raw => "extra_body",
            Self::Config => "config",
            Self::Provider => "provider default",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Effective {
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) source: Source,
    pub(crate) adaptive: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ControlError(pub(crate) &'static str);
impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ControlError {}
#[derive(Clone, Copy)]
struct Claude {
    adaptive: bool,
    effort_supported: bool,
    manual: bool,
    xhigh: bool,
    max: bool,
    off: Off,
}
#[derive(Clone, Copy)]
enum Off {
    Yes,
    ThroughHigh,
    No,
}

// Exact published model families plus their YYYYMMDD version aliases. A
// Claude-looking custom model name does not establish API capabilities.
fn claude(model: &str) -> Option<Claude> {
    let families = [
        ("claude-opus-5-5", true, true, true, Off::No),
        ("claude-sonnet-5-5", true, true, true, Off::No),
        ("claude-haiku-5-5", true, true, true, Off::ThroughHigh),
        ("claude-fable-5-1", true, true, true, Off::No),
        ("claude-mythos-5-1", true, true, true, Off::No),
        ("claude-fable-5", true, true, true, Off::No),
        ("claude-mythos-5", true, true, true, Off::No),
        ("claude-opus-5", true, true, true, Off::ThroughHigh),
        ("claude-sonnet-5", true, true, true, Off::Yes),
        ("claude-opus-4-8", true, true, true, Off::Yes),
        ("claude-opus-4-7", true, true, true, Off::Yes),
        ("claude-opus-4-6", true, false, true, Off::Yes),
        ("claude-sonnet-4-6", true, false, true, Off::Yes),
        ("claude-opus-4-5", false, false, false, Off::Yes),
        ("claude-sonnet-4-5", false, false, false, Off::Yes),
        ("claude-haiku-4-5", false, false, false, Off::Yes),
    ];
    families
        .into_iter()
        .find_map(|(name, adaptive, xhigh, max, off)| {
            let matched = model == name
                || model
                    .strip_prefix(name)
                    .and_then(|s| s.strip_prefix('-'))
                    .is_some_and(|date| {
                        date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit())
                    });
            matched.then_some(Claude {
                adaptive,
                effort_supported: adaptive || name == "claude-opus-4-5",
                manual: !adaptive || matches!(name, "claude-opus-4-6" | "claude-sonnet-4-6"),
                xhigh,
                max,
                off,
            })
        })
}
fn effort(value: &Value) -> Result<ReasoningEffort, ControlError> {
    serde_json::from_value(value.clone())
        .map_err(|_| ControlError("invalid reasoning effort; use low, medium, high, xhigh or max"))
}
fn object<'a>(body: &'a mut Value, key: &str) -> Result<&'a mut Map<String, Value>, ControlError> {
    let value = body
        .as_object_mut()
        .ok_or(ControlError(
            "request controls require an object extra_body",
        ))?
        .entry(key)
        .or_insert_with(|| json!({}));
    value.as_object_mut().ok_or(ControlError(
        "reasoning controls require object-valued request fields",
    ))
}
fn off_allowed(profile: Claude, level: Option<ReasoningEffort>) -> bool {
    match profile.off {
        Off::Yes => true,
        Off::No => false,
        Off::ThroughHigh => !matches!(level, Some(ReasoningEffort::Xhigh | ReasoningEffort::Max)),
    }
}

pub(crate) fn apply(
    protocol: Protocol,
    model: &str,
    configured: Option<ReasoningEffort>,
    session: SessionReasoning,
    params: Option<Value>,
) -> Result<(Option<Value>, Effective), ControlError> {
    let mut body = params.unwrap_or(Value::Null);
    let gateway_model;
    let native_model = match protocol {
        Protocol::Anthropic => Some(body.get("model").and_then(Value::as_str).unwrap_or(model)),
        Protocol::OpenRouter => {
            let name = body.get("model").and_then(Value::as_str).unwrap_or(model);
            gateway_model = name
                .strip_prefix("anthropic/")
                .map(|name| name.replace('.', "-"));
            gateway_model.as_deref()
        }
        _ => None,
    };
    let key = match protocol {
        Protocol::Anthropic => "output_config",
        Protocol::OpenRouter => "reasoning",
        Protocol::OpenAiResponses | Protocol::OpenAiCompletions => "reasoning",
        Protocol::Other => "",
    };
    let raw = if matches!(
        protocol,
        Protocol::OpenAiResponses | Protocol::OpenAiCompletions
    ) {
        body.get("reasoning")
            .and_then(|v| v.get("effort"))
            .or_else(|| body.get("reasoning_effort"))
    } else {
        body.get(key).and_then(|v| v.get("effort"))
    };
    let raw = raw.map(effort).transpose()?;
    let (level, source) = if let Some(value) = session.effort {
        (Some(value), Source::Session)
    } else if let Some(value) = raw {
        (Some(value), Source::Raw)
    } else if let Some(value) = configured {
        (Some(value), Source::Config)
    } else {
        (None, Source::Provider)
    };
    let explicit = session != SessionReasoning::default();
    if native_model.is_none() {
        if matches!(
            protocol,
            Protocol::OpenAiResponses | Protocol::OpenAiCompletions
        ) {
            if level == Some(ReasoningEffort::Max) {
                return Err(ControlError(
                    "max effort is not supported by this OpenAI request path",
                ));
            }
            if session.thinking.is_some() {
                return Err(ControlError(
                    "this provider uses /reasoning as a prompt preference",
                ));
            }
            if session.effort.is_some() {
                if body.is_null() {
                    body = json!({});
                }
                if protocol == Protocol::OpenAiCompletions {
                    body["reasoning_effort"] = json!(level.unwrap().as_wire_str());
                } else {
                    object(&mut body, "reasoning")?
                        .insert("effort".into(), json!(level.unwrap().as_wire_str()));
                }
            }
            return Ok((
                (!body.is_null()).then_some(body),
                Effective {
                    effort: level,
                    source,
                    adaptive: false,
                },
            ));
        }
        if explicit {
            return Err(ControlError(
                "this model route does not expose supported session effort controls",
            ));
        }
        return Ok((
            (!body.is_null()).then_some(body),
            Effective {
                effort: None,
                source: Source::Provider,
                adaptive: false,
            },
        ));
    }
    let native_model = native_model.unwrap();
    let profile = claude(native_model);
    let raw_thinking = body.get("thinking").is_some();
    if level.is_none() && !explicit && !raw_thinking {
        return Ok((
            (!body.is_null()).then_some(body),
            Effective {
                effort: None,
                source,
                adaptive: false,
            },
        ));
    }
    let profile=profile.ok_or(ControlError("this Claude model has no verified effort/thinking compatibility; clear the session override or use a supported model"))?;
    if let Some(level) = level {
        if !profile.effort_supported {
            return Err(ControlError("effort is unsupported by this Claude model"));
        }
        match level {
            ReasoningEffort::None | ReasoningEffort::Minimal => {
                return Err(ControlError(
                    "Claude effort supports low, medium, high, xhigh or max; use /reasoning off where supported",
                ));
            }
            ReasoningEffort::Xhigh if !profile.xhigh => {
                return Err(ControlError(
                    "xhigh effort is not supported by this Claude model",
                ));
            }
            ReasoningEffort::Max if !profile.max => {
                return Err(ControlError(
                    "max effort is not supported by this Claude model",
                ));
            }
            _ => {}
        }
    }
    if body.is_null() {
        body = json!({});
    }
    if protocol == Protocol::OpenRouter {
        let fields = object(&mut body, "reasoning")?;
        if let Some(level) = level {
            if session.effort.is_some() {
                fields.remove("max_tokens");
            }
            if fields.contains_key("max_tokens") {
                return Err(ControlError(
                    "choose reasoning effort or a reasoning token budget, not both",
                ));
            }
            fields.insert("effort".into(), json!(level.as_wire_str()));
        }
        if let Some(mode) = session.thinking {
            if mode == ThinkingMode::Off && !off_allowed(profile, level) {
                return Err(ControlError(
                    "thinking cannot be disabled at this model/effort; use /effort low to reduce reasoning",
                ));
            }
            fields.insert("enabled".into(), json!(mode == ThinkingMode::On));
        } else if session.effort.is_some() {
            fields.insert("enabled".into(), json!(true));
        }
        if fields.get("enabled") == Some(&Value::Bool(false)) && !off_allowed(profile, level) {
            return Err(ControlError(
                "thinking cannot be disabled at this model/effort; use /effort low to reduce reasoning",
            ));
        }
        return Ok((
            Some(body),
            Effective {
                effort: level,
                source,
                adaptive: false,
            },
        ));
    }
    if let Some(level) = level {
        object(&mut body, "output_config")?.insert("effort".into(), json!(level.as_wire_str()));
    }
    let requested = session
        .thinking
        .or_else(|| (session.effort.is_some() && profile.adaptive).then_some(ThinkingMode::On));
    if let Some(mode) = requested {
        if mode == ThinkingMode::Off && !off_allowed(profile, level) {
            return Err(ControlError(
                "thinking cannot be disabled at this model/effort; use /effort low to reduce reasoning",
            ));
        }
        let fields = object(&mut body, "thinking")?;
        if mode == ThinkingMode::Off {
            fields.insert("type".into(), json!("disabled"));
            fields.remove("budget_tokens");
        } else if profile.adaptive {
            fields.insert("type".into(), json!("adaptive"));
            fields.remove("budget_tokens");
        } else if fields.get("type").and_then(Value::as_str) != Some("enabled")
            || fields
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .is_none()
        {
            return Err(ControlError(
                "this model requires an explicit thinking budget in extra_body; adaptive thinking is unsupported",
            ));
        }
    } else if level.is_some() && profile.adaptive && !raw_thinking {
        object(&mut body, "thinking")?.insert("type".into(), json!("adaptive"));
    }
    let mode = body
        .get("thinking")
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str);
    match mode {
        Some("disabled") if !off_allowed(profile, level) => {
            return Err(ControlError(
                "thinking cannot be disabled at this model/effort; use /effort low to reduce reasoning",
            ));
        }
        Some("adaptive") if !profile.adaptive => {
            return Err(ControlError(
                "adaptive thinking is unsupported by this Claude model",
            ));
        }
        Some("enabled") if !profile.manual => {
            return Err(ControlError(
                "manual thinking is unsupported by this Claude model; use adaptive thinking",
            ));
        }
        Some("adaptive") if body["thinking"].get("budget_tokens").is_some() => {
            return Err(ControlError(
                "adaptive thinking does not accept a manual token budget",
            ));
        }
        Some("enabled" | "adaptive" | "disabled") | None => {}
        Some(_) => {
            return Err(ControlError(
                "unsupported thinking mode; use adaptive, enabled or disabled",
            ));
        }
    }
    let adaptive = mode == Some("adaptive");
    Ok((
        Some(body),
        Effective {
            effort: level,
            source,
            adaptive,
        },
    ))
}

/// The model's actual default or explicit request mode, independent of the
/// historical prompt-style/display preference. No unknown model is guessed.
pub(crate) fn thinking_enabled(
    protocol: Protocol,
    model: &str,
    params: Option<&Value>,
) -> Option<bool> {
    let normalized;
    let name = match protocol {
        Protocol::Anthropic => params
            .and_then(|v| v.get("model"))
            .and_then(Value::as_str)
            .unwrap_or(model),
        Protocol::OpenRouter => {
            normalized = params
                .and_then(|v| v.get("model"))
                .and_then(Value::as_str)
                .unwrap_or(model)
                .strip_prefix("anthropic/")?
                .replace('.', "-");
            normalized.as_str()
        }
        _ => return None,
    };
    let profile = claude(name)?;
    if protocol == Protocol::OpenRouter {
        if let Some(enabled) = params
            .and_then(|v| v.pointer("/reasoning/enabled"))
            .and_then(Value::as_bool)
        {
            return Some(enabled);
        }
        if params
            .and_then(|v| v.pointer("/reasoning/effort"))
            .is_some()
        {
            return Some(true);
        }
    }
    match params
        .and_then(|v| v.pointer("/thinking/type"))
        .and_then(Value::as_str)
    {
        Some("adaptive" | "enabled") => Some(true),
        Some("disabled") => Some(false),
        Some(_) => None,
        None => Some(
            profile.adaptive
                && !name.starts_with("claude-opus-4")
                && !name.starts_with("claude-sonnet-4"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn effort_native_levels_and_schema_preserve_defaults_and_exact_routes() {
        let (params, eff) = apply(
            Protocol::Anthropic,
            "claude-opus-5-5",
            None,
            SessionReasoning::default(),
            None,
        )
        .unwrap();
        assert_eq!(params, None);
        assert_eq!(eff.source, Source::Provider);
        for level in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
        ] {
            let (p,e)=apply(Protocol::Anthropic,"claude-opus-5-5",Some(level),SessionReasoning::default(),Some(json!({"output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}}))).unwrap();
            let p = p.unwrap();
            assert_eq!(p["output_config"]["effort"], level.as_wire_str());
            assert_eq!(p["output_config"]["format"]["schema"]["type"], "object");
            assert_eq!(p["thinking"], json!({"type":"adaptive"}));
            assert!(e.adaptive);
        }
        assert!(
            apply(
                Protocol::Anthropic,
                "custom-claude-opus-5-5",
                Some(ReasoningEffort::High),
                SessionReasoning::default(),
                None
            )
            .is_err()
        );
        assert!(
            apply(
                Protocol::Other,
                "claude-opus-5-5",
                None,
                SessionReasoning {
                    effort: Some(ReasoningEffort::High),
                    thinking: None
                },
                None
            )
            .is_err()
        );
    }
    #[test]
    fn effort_model_compatibility_and_explicit_off_never_silently_clamp() {
        let session = |effort, thinking| SessionReasoning { effort, thinking };
        assert!(
            apply(
                Protocol::Anthropic,
                "claude-opus-4-6",
                None,
                session(Some(ReasoningEffort::Xhigh), None),
                None
            )
            .is_err()
        );
        assert!(
            apply(
                Protocol::Anthropic,
                "claude-opus-4-5",
                None,
                session(Some(ReasoningEffort::Max), None),
                None
            )
            .is_err()
        );
        assert!(
            apply(
                Protocol::Anthropic,
                "claude-opus-5-5",
                None,
                session(None, Some(ThinkingMode::Off)),
                None
            )
            .is_err()
        );
        assert!(
            apply(
                Protocol::Anthropic,
                "claude-sonnet-5-5",
                None,
                session(None, Some(ThinkingMode::Off)),
                None
            )
            .is_err()
        );
        assert!(
            apply(
                Protocol::Anthropic,
                "claude-opus-5",
                None,
                session(Some(ReasoningEffort::Max), Some(ThinkingMode::Off)),
                None
            )
            .is_err()
        );
        let (p, _) = apply(
            Protocol::Anthropic,
            "claude-sonnet-4-6",
            None,
            session(None, Some(ThinkingMode::Off)),
            None,
        )
        .unwrap();
        assert_eq!(p.unwrap()["thinking"]["type"], "disabled");
        let (p, _) = apply(
            Protocol::Anthropic,
            "claude-opus-4-5-20251101",
            Some(ReasoningEffort::High),
            SessionReasoning::default(),
            None,
        )
        .unwrap();
        assert!(p.unwrap().get("thinking").is_none());
    }
    #[test]
    fn effort_raw_precedence_session_override_gateway_shape_and_no_forced_display() {
        let raw = json!({"output_config":{"effort":"low","format":{"type":"json_schema"}},"thinking":{"type":"disabled","display":"omitted"}});
        let (p, e) = apply(
            Protocol::Anthropic,
            "claude-sonnet-4-6",
            Some(ReasoningEffort::High),
            SessionReasoning::default(),
            Some(raw.clone()),
        )
        .unwrap();
        assert_eq!(e.source, Source::Raw);
        assert_eq!(p.unwrap()["thinking"]["type"], "disabled");
        let (p, e) = apply(
            Protocol::Anthropic,
            "claude-sonnet-4-6",
            Some(ReasoningEffort::High),
            SessionReasoning {
                effort: Some(ReasoningEffort::Max),
                thinking: None,
            },
            Some(raw),
        )
        .unwrap();
        let p = p.unwrap();
        assert_eq!(e.source, Source::Session);
        assert_eq!(
            p["thinking"],
            json!({"type":"adaptive","display":"omitted"})
        );
        assert_eq!(p["output_config"]["format"]["type"], "json_schema");
        let(p,_)=apply(Protocol::OpenRouter,"anthropic/claude-opus-5.5",None,SessionReasoning{effort:Some(ReasoningEffort::Max),thinking:None},Some(json!({"reasoning":{"effort":"low","max_tokens":1000},"provider":{"order":["anthropic"]}}))).unwrap();
        let p = p.unwrap();
        assert_eq!(p["reasoning"], json!({"effort":"max","enabled":true}));
        assert!(p.get("thinking").is_none());
        assert!(p.get("output_config").is_none());
        assert_eq!(p["provider"]["order"][0], "anthropic");
    }
}
