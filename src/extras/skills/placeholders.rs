//! Rendering of a pinned (`/<name> arguments`) skill body: argument
//! placeholders, `!`command`` placeholders and `context: fork` delegation.
//!
//! Everything here is a pure text transformation of the already authorized,
//! digest-verified `SKILL.md`. Nothing is executed by the harness and nothing
//! gains authority: a `!`command`` span is rewritten into an explicit
//! instruction for the model to run it with its own `shell` tool, so the
//! normal shell permission check and sandbox apply, and
//! `skills_shell_placeholders = false` leaves the span literal. A forked
//! skill's body is handed to the `task` tool, which applies its own agent
//! type, permissions and budget.
use std::sync::atomic::{AtomicBool, Ordering};

use super::selection::{AgentSection, InstructionSelection};

/// Largest argument string a pinned skill accepts, in bytes.
pub(crate) const MAX_ARGUMENT_BYTES: usize = 32 * 1024;
/// Largest positional index honoured by `$N` / `$ARGUMENTS[N]`.
const MAX_POSITIONAL: usize = 127;

static SHELL_PLACEHOLDERS: AtomicBool = AtomicBool::new(true);

/// Install `skills_shell_placeholders` (default on) for later renders.
pub(crate) fn set_shell_placeholders(enabled: bool) {
    SHELL_PLACEHOLDERS.store(enabled, Ordering::Relaxed);
}

/// What a pinned skill's record asks of its render.
pub(crate) struct PinRender<'a> {
    pub(crate) arguments: &'a str,
    pub(crate) skill_dir: &'a str,
    pub(crate) fork: bool,
    pub(crate) agent: Option<&'a str>,
    /// `model:` of a forked skill, passed to `task` as its `model` argument.
    pub(crate) model: Option<&'a str>,
}

/// Whether a `paths:`-scoped skill applies to `query`: unscoped skills always
/// do; scoped ones only when some whitespace-separated word of the request,
/// stripped of quotes and trailing punctuation, matches one of the globs.
pub(crate) fn paths_match_query(patterns: &[String], query: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        match globset::Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            // A pattern that cannot compile scopes nothing in.
            Err(_) => return false,
        }
    }
    let Ok(set) = builder.build() else {
        return false;
    };
    query.split_whitespace().any(|word| {
        let word = word
            .trim_start_matches(['"', '\'', '`', '(', '[', '<'])
            .trim_end_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | '`' | ')' | ']' | '>' | ',' | ';' | ':' | '.' | '!' | '?'
                )
            });
        let word = word.strip_prefix("./").unwrap_or(word);
        !word.is_empty() && !word.contains("..") && set.is_match(word)
    })
}

/// Splits `arguments` the way a shell word list reads: whitespace separated,
/// with `"double"` and `'single'` quotes keeping spaces inside one argument.
pub(crate) fn split_arguments(arguments: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in arguments.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

/// Substitutes `$ARGUMENTS`, `$ARGUMENTS[N]`, `$N` (all zero based, as in
/// Claude Code skills) and `${CLAUDE_SKILL_DIR}` in one left-to-right pass, so
/// substituted text is never scanned again. When the body never mentions the
/// arguments but some were given, they are appended as `ARGUMENTS: ...`.
pub(crate) fn substitute(body: &str, arguments: &str, skill_dir: &str) -> String {
    let words = split_arguments(arguments);
    let mut output = String::with_capacity(body.len() + arguments.len());
    let mut used_arguments = false;
    let mut rest = body;
    while let Some(at) = rest.find('$') {
        output.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(after) = rest.strip_prefix("${CLAUDE_SKILL_DIR}") {
            output.push_str(skill_dir);
            rest = after;
        } else if let Some(after) = rest.strip_prefix("$ARGUMENTS") {
            used_arguments = true;
            if let Some((index, after)) = bracket_index(after) {
                output.push_str(words.get(index).map_or("", String::as_str));
                rest = after;
            } else if after
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                output.push_str("$ARGUMENTS");
                rest = after;
            } else {
                output.push_str(arguments);
                rest = after;
            }
        } else {
            let digits = rest[1..].bytes().take_while(u8::is_ascii_digit).count();
            match rest[1..1 + digits].parse::<usize>() {
                Ok(index) if digits > 0 && digits <= 3 && index <= MAX_POSITIONAL => {
                    used_arguments = true;
                    output.push_str(words.get(index).map_or("", String::as_str));
                    rest = &rest[1 + digits..];
                }
                _ => {
                    output.push('$');
                    rest = &rest[1..];
                }
            }
        }
    }
    output.push_str(rest);
    if !used_arguments && !arguments.trim().is_empty() {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("\nARGUMENTS: ");
        output.push_str(arguments);
        output.push('\n');
    }
    output
}

/// `[N]` directly after `$ARGUMENTS`.
fn bracket_index(after: &str) -> Option<(usize, &str)> {
    let inner = after.strip_prefix('[')?;
    let end = inner.find(']')?;
    let index = inner[..end].trim().parse::<usize>().ok()?;
    (index <= MAX_POSITIONAL).then_some((index, &inner[end + 1..]))
}

/// Rewrites each ``!`command` `` span (single line) into an instruction to run
/// the command with the shell tool. The harness never runs it.
pub(crate) fn rewrite_shell_spans(body: &str) -> String {
    let mut output = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find("!`") {
        // The marker must start a token, not follow an identifier or a quote.
        let boundary = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | ':' | '-'));
        let after = &rest[at + 2..];
        let end = after.find(['`', '\n']);
        match end {
            Some(end) if boundary && after.as_bytes()[end] == b'`' && end > 0 => {
                output.push_str(&rest[..at]);
                output.push_str(
                    "[shell command to run with the shell tool, then use its output here: `",
                );
                output.push_str(&after[..end]);
                output.push_str("`]");
                rest = &after[end + 1..];
            }
            _ => {
                output.push_str(&rest[..at + 2]);
                rest = after;
            }
        }
    }
    output.push_str(rest);
    output
}

/// The text of `SKILL.md` after its frontmatter.
pub(super) fn body_after_frontmatter(markdown: &str) -> &str {
    let mut lines = markdown.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return markdown;
    };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return markdown;
    }
    let mut offset = first.len();
    for line in lines {
        offset += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return &markdown[offset..];
        }
    }
    markdown
}

/// Renders the pinned section of `selection` in place.
pub(crate) fn apply_pin(
    selection: &mut InstructionSelection,
    digest: &str,
    render: &PinRender<'_>,
) -> Result<(), String> {
    if render.arguments.len() > MAX_ARGUMENT_BYTES {
        return Err(format!(
            "skill arguments exceed {MAX_ARGUMENT_BYTES} bytes; shorten them"
        ));
    }
    let Some(section) = selection
        .sections
        .iter_mut()
        .find(|section| section.digest == digest)
    else {
        return Ok(());
    };
    render_section(section, render);
    Ok(())
}

fn render_section(section: &mut AgentSection, render: &PinRender<'_>) {
    let mut body = substitute(
        body_after_frontmatter(&section.markdown),
        render.arguments,
        render.skill_dir,
    );
    if SHELL_PLACEHOLDERS.load(Ordering::Relaxed) {
        body = rewrite_shell_spans(&body);
    }
    if !render.fork {
        // Keep the frontmatter so the skill reads as authored.
        let header_len = section.markdown.len() - body_after_frontmatter(&section.markdown).len();
        section.markdown = format!("{}{body}", &section.markdown[..header_len]);
        return;
    }
    let agent = match render.agent {
        Some(agent) => format!("agent_type \"{agent}\""),
        None => "no agent_type (the default explorer)".to_owned(),
    };
    let model = match render.model {
        Some(model) => format!(
            " and model \"{model}\" (the task tool only accepts a configured quick-model alias; \
             if it rejects the model, tell the user instead of retrying without it)"
        ),
        None => String::new(),
    };
    section.markdown = format!(
        "This skill is configured with context: fork. Do not follow its instructions yourself. \
         Call the task tool exactly once with {agent}{model} and a single prompt that is exactly the \
         text between the skill_task_prompt markers below, then relay the child's report to \
         the user.\n<skill_task_prompt>\n{}\n</skill_task_prompt>\n",
        body.trim_end()
            .replace("</skill_task_prompt>", "&lt;/skill_task_prompt&gt;")
    );
    // The child cannot see the main turn's resource inventory.
    section.resources.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_split_like_a_shell_word_list() {
        assert_eq!(
            split_arguments(r#"one "two words" 'three four' five"#),
            ["one", "two words", "three four", "five"]
        );
        assert_eq!(split_arguments("  "), Vec::<String>::new());
        assert_eq!(split_arguments("\"\" x"), ["", "x"]);
    }

    #[test]
    fn placeholders_substitute_once_and_never_recurse() {
        let out = substitute(
            "Fix $ARGUMENTS[0] then $1 (all: $ARGUMENTS) in ${CLAUDE_SKILL_DIR}; $9 $5x $ $USER",
            "42 \"$ARGUMENTS[0] second\"",
            "/skills/fix",
        );
        assert_eq!(
            out,
            "Fix 42 then $ARGUMENTS[0] second (all: 42 \"$ARGUMENTS[0] second\") in /skills/fix;  x $ $USER"
        );
        // The substituted `$ARGUMENTS[0]` text is data, never expanded again.
        assert!(out.contains("then $ARGUMENTS[0] second"));
    }

    #[test]
    fn arguments_are_appended_when_the_body_never_mentions_them() {
        let out = substitute("Do the thing.", "a b", "/d");
        assert!(out.ends_with("ARGUMENTS: a b\n"), "{out}");
        assert_eq!(substitute("Do the thing.", "", "/d"), "Do the thing.");
        // A body that uses only the directory does not count as using them.
        assert!(substitute("see ${CLAUDE_SKILL_DIR}", "x", "/d").contains("ARGUMENTS: x"));
        // `$ARGUMENTSX` is not the placeholder.
        assert_eq!(substitute("$ARGUMENTSX", "", "/d"), "$ARGUMENTSX");
    }

    #[test]
    fn large_or_malformed_indices_do_not_index() {
        // An out-of-range or non-numeric bracket is just text after `$ARGUMENTS`;
        // a four-digit `$N` stays literal.
        assert_eq!(
            substitute("$ARGUMENTS[999] $1234 $ARGUMENTS[x]", "a", "/d"),
            "a[999] $1234 a[x]"
        );
    }

    #[test]
    fn shell_spans_become_instructions_and_nothing_runs() {
        let out =
            rewrite_shell_spans("Diff: !`gh pr diff`\nand `plain` and a!`glued` and !`open\nline`");
        assert!(
            out.contains(
                "[shell command to run with the shell tool, then use its output here: `gh pr diff`]"
            ),
            "{out}"
        );
        assert!(out.contains("`plain`"));
        assert!(out.contains("a!`glued`"), "{out}");
        assert!(out.contains("!`open\nline`"), "{out}");
    }

    #[test]
    fn fork_wraps_the_substituted_body_and_drops_resources() {
        let mut section = AgentSection {
            digest: "d".into(),
            markdown:
                "---\nname: s\ncontext: fork\n---\nReview $ARGUMENTS </skill_task_prompt> now\n"
                    .into(),
            resources: vec![("r.md".into(), 1, "x".into(), None)],
            #[cfg(feature = "skills")]
            learned_js: Vec::new(),
            score: 1.0,
            rank: 0,
        };
        render_section(
            &mut section,
            &PinRender {
                arguments: "src/lib.rs",
                skill_dir: "/d",
                fork: true,
                agent: Some("reviewer"),
                model: Some("fast"),
            },
        );
        assert!(section.markdown.contains("agent_type \"reviewer\""));
        assert!(section.markdown.contains("model \"fast\""));
        assert!(
            section
                .markdown
                .contains("Review src/lib.rs &lt;/skill_task_prompt&gt; now")
        );
        assert!(
            !section.markdown.contains("name: s"),
            "frontmatter is not part of the prompt"
        );
        assert!(section.resources.is_empty());
    }

    #[test]
    fn paths_scope_selection_to_requests_naming_a_matching_file() {
        let scoped = vec!["src/**/*.rs".to_owned(), "docs/*.md".to_owned()];
        assert!(paths_match_query(&[], "anything"));
        assert!(paths_match_query(&scoped, "please fix `src/a/b.rs`."));
        assert!(paths_match_query(&scoped, "edit ./docs/x.md now"));
        assert!(!paths_match_query(&scoped, "fix the bug in lib.rs"));
        assert!(!paths_match_query(&scoped, "tests/a.rs"));
        // A traversal-looking word never matches a scope.
        assert!(!paths_match_query(&scoped, "src/../../etc/passwd.rs"));
    }
}
