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

/// Parses the JSON value found at the top-level `hooks` key of settings.json.
pub(crate) fn parse_hooks_config(value: &serde_json::Value) -> Result<HooksConfig, String> {
    let mut config: HooksConfig =
        serde_json::from_value(value.clone()).map_err(|e| format!("invalid hooks config: {e}"))?;

    for (event, groups) in &mut config {
        for group in groups {
            for handler in &mut group.hooks {
                if handler.kind != "command" {
                    return Err(format!(
                        "unsupported hook type {:?} for event {event:?} (only \"command\" is supported in v1)",
                        handler.kind
                    ));
                }
                if handler.args.is_none()
                    && let Some(command) = handler.command.as_deref()
                {
                    let mut argv = split_command_string(command)?;
                    // Preserve existing single-program bindings (including
                    // Windows backslashes) and their persisted trust identity.
                    if argv.len() == 1
                        && !command
                            .chars()
                            .any(|ch| ch.is_whitespace() || matches!(ch, '\'' | '"'))
                    {
                        continue;
                    }
                    handler.command = Some(argv.remove(0));
                    handler.args = (!argv.is_empty()).then_some(argv);
                }
            }
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
