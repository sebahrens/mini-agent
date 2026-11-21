//! Claude Code style hook `if` rules (`Bash(git push *)`, `Edit`, `Read(./.env)`).
//!
//! A hook's `if` is normally shell text run with `sh -c`. Claude Code's `if`
//! is instead a permission-rule expression evaluated in process. A condition
//! that has exactly the rule shape is compiled here and matched against the
//! event envelope without launching a subprocess; everything else stays shell
//! text (mini-agent-nvnkx.180).

use super::normalize::canonical_tool_name;

/// A parsed `Tool` or `Tool(pattern)` rule.
#[derive(Debug, PartialEq, Eq)]
struct IfRule<'a> {
    tool: &'a str,
    pattern: Option<&'a str>,
}

/// Recognises the rule shape. A bare name must look like a tool name
/// (initial capital letter or `mcp__`), so shell words such as `true` or
/// `./check.sh` are never mistaken for a rule; a parenthesised form is already
/// a shell syntax error, so any identifier is accepted.
fn parse_rule(condition: &str) -> Option<IfRule<'_>> {
    let text = condition.trim();
    let name_len = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(text.len());
    let (tool, rest) = text.split_at(name_len);
    if tool.is_empty() || tool.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    if rest.is_empty() {
        let looks_like_tool =
            tool.starts_with(|c: char| c.is_ascii_uppercase()) || tool.starts_with("mcp__");
        return looks_like_tool.then_some(IfRule {
            tool,
            pattern: None,
        });
    }
    let pattern = rest.strip_prefix('(')?.strip_suffix(')')?;
    Some(IfRule {
        tool,
        pattern: Some(pattern),
    })
}

/// Evaluates `condition` as a rule against the hook envelope. `None` means
/// the text is not a rule and must be run as shell text.
pub(crate) fn evaluate(condition: &str, envelope: &serde_json::Value) -> Option<bool> {
    let rule = parse_rule(condition)?;
    let Some(tool_name) = envelope.get("tool_name").and_then(|name| name.as_str()) else {
        return Some(false);
    };
    if !tool_matches(rule.tool, tool_name) {
        return Some(false);
    }
    let Some(pattern) = rule.pattern else {
        return Some(true);
    };
    if pattern.is_empty() || pattern == "*" {
        return Some(true);
    }
    let Some(input) = primary_input(envelope.get("tool_input")) else {
        return Some(false);
    };
    // Claude Code's legacy `Bash(npm:*)` prefix form.
    let matched = match pattern.strip_suffix(":*") {
        Some(prefix) => {
            input == prefix
                || input
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with(' '))
        }
        None => wildcard_match(pattern, &input),
    };
    Some(matched)
}

fn tool_matches(rule_tool: &str, tool_name: &str) -> bool {
    let rule = canonical_tool_name(rule_tool);
    let actual = canonical_tool_name(tool_name);
    let is_edit = |name: &str| matches!(name, "edit" | "write" | "multi_edit" | "notebook_edit");
    rule == actual || (is_edit(&rule) && is_edit(&actual))
}

/// The input a rule pattern is matched against: the command, the path or the
/// URL of the call.
fn primary_input(input: Option<&serde_json::Value>) -> Option<String> {
    let input = input?;
    if let Some(text) = input.as_str() {
        return Some(text.to_owned());
    }
    ["command", "path", "file_path", "url", "pattern", "query"]
        .iter()
        .find_map(|key| input.get(key).and_then(|value| value.as_str()))
        .map(str::to_owned)
}

/// `*` matches any run of characters; everything else is literal.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            backtrack = Some((p, t));
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((star, matched)) = backtrack {
            p = star + 1;
            t = matched + 1;
            backtrack = Some((star, matched + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(tool: &str, input: serde_json::Value) -> serde_json::Value {
        json!({"tool_name": tool, "tool_input": input})
    }

    #[test]
    fn rules_match_canonical_tool_and_primary_input() {
        let rm = envelope("bash", json!({"command": "rm -rf x"}));
        let ls = envelope("bash", json!({"command": "ls"}));
        assert_eq!(evaluate("Bash(rm *)", &rm), Some(true));
        assert_eq!(evaluate("Bash(rm *)", &ls), Some(false));
        assert_eq!(evaluate("Bash(rm:*)", &rm), Some(true));
        assert_eq!(evaluate("Bash", &ls), Some(true));
        assert_eq!(
            evaluate("Edit", &envelope("write", json!({"path": "a"}))),
            Some(true)
        );
        assert_eq!(
            evaluate(
                "Write(src/*.rs)",
                &envelope("write", json!({"path": "src/a.rs"}))
            ),
            Some(true)
        );
        assert_eq!(evaluate("Read", &ls), Some(false));
        assert_eq!(evaluate("Bash(rm *)", &json!({})), Some(false));
    }

    #[test]
    fn shell_text_is_not_a_rule() {
        for text in [
            "true",
            "test -d src",
            "./check.sh",
            "exit 1",
            "git status | grep x",
        ] {
            assert_eq!(parse_rule(text), None, "{text}");
        }
    }
}
