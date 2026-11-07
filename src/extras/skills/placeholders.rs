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
use std::path::Path;
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
    /// Whether the turn advertises the `task` tool. A forked skill renders
    /// inline (with a notice) when it does not.
    pub(crate) task_available: bool,
    pub(crate) agent: Option<&'a str>,
    /// `model:` of a forked skill, passed to `task` as its `model` argument.
    pub(crate) model: Option<&'a str>,
}

/// Longest prefix of a request scanned for file names, so a pasted log cannot
/// make scoping quadratic while a path after the 8 KiB retrieval cut still counts.
const PATH_SCAN_LIMIT: usize = 256 * 1024;
/// Longest single word considered a path.
const PATH_WORD_LIMIT: usize = 1024;

/// Whether a `paths:`-scoped skill applies: unscoped skills always do; scoped
/// ones when (a) some whitespace-separated word of `request` names a matching
/// file (relative, `@`-mention, `./`, absolute under `root`, or a bare filename
/// whose name fits a pattern's last segment), or (b) a file in `touched`
/// (workspace-relative, read/edited/listed earlier in the session) matches.
pub(crate) fn paths_match_request(
    patterns: &[String],
    request: &str,
    root: &Path,
    touched: &[String],
) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let mut builder = globset::GlobSetBuilder::new();
    let mut name_builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        match globset::Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            // A pattern that cannot compile scopes nothing in.
            Err(_) => return false,
        }
        // Bare filenames are tested against the pattern's final segment, except
        // for catch-all segments that would match any word.
        let last = pattern.rsplit('/').next().unwrap_or(pattern);
        if !matches!(last, "" | "*" | "**")
            && let Ok(glob) = globset::Glob::new(last)
        {
            name_builder.add(glob);
        }
    }
    let (Ok(set), Ok(names)) = (builder.build(), name_builder.build()) else {
        return false;
    };
    if touched.iter().any(|file| set.is_match(file.as_str())) {
        return true;
    }
    let mut end = request.len().min(PATH_SCAN_LIMIT);
    while !request.is_char_boundary(end) {
        end -= 1;
    }
    request[..end].split_whitespace().any(|word| {
        if word.len() > PATH_WORD_LIMIT {
            return false;
        }
        let word = word
            .trim_start_matches(['"', '\'', '`', '(', '[', '<', '@'])
            .trim_end_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | '`' | ')' | ']' | '>' | ',' | ';' | ':' | '.' | '!' | '?'
                )
            });
        let Some(relative) = super::touched::relative_to_workspace(root, Path::new(word)) else {
            return false;
        };
        set.is_match(relative.as_str())
            || (!relative.contains('/')
                && relative.trim_start_matches('.').contains('.')
                && names.is_match(relative.as_str()))
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
///
/// Positional `$N` is substituted only when arguments were supplied, and never
/// inside fenced code blocks (so `awk '{print $1}'` survives). `$$N`,
/// `$$ARGUMENTS` and `$${CLAUDE_SKILL_DIR}` escape to a literal `$N`,
/// `$ARGUMENTS` and `${CLAUDE_SKILL_DIR}`.
pub(crate) fn substitute(body: &str, arguments: &str, skill_dir: &str) -> String {
    let words = split_arguments(arguments);
    let positional = !arguments.trim().is_empty();
    let fences = fenced_ranges(body);
    let mut output = String::with_capacity(body.len() + arguments.len());
    let mut used_arguments = false;
    let mut rest = body;
    while let Some(at) = rest.find('$') {
        output.push_str(&rest[..at]);
        rest = &rest[at..];
        let offset = body.len() - rest.len();
        if let Some(after) = rest.strip_prefix("$$")
            && (after.starts_with(|c: char| c.is_ascii_digit())
                || after.starts_with("ARGUMENTS")
                || after.starts_with("{CLAUDE_SKILL_DIR}"))
        {
            // Escape: `$$1` is a literal `$1`; the token is copied raw.
            output.push('$');
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${CLAUDE_SKILL_DIR}") {
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
                Ok(index)
                    if positional
                        && digits > 0
                        && digits <= 3
                        && index <= MAX_POSITIONAL
                        && !fences.iter().any(|r| r.contains(&offset)) =>
                {
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

/// Byte ranges of fenced code blocks (``` or ~~~), fence lines included. An
/// unterminated fence runs to the end of the body.
fn fenced_ranges(body: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut open: Option<(usize, &str)> = None;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m));
        match (open, marker) {
            (None, Some(m)) => open = Some((offset, m)),
            (Some((start, open_marker)), Some(m)) if m == open_marker => {
                ranges.push(start..offset + line.len());
                open = None;
            }
            _ => {}
        }
        offset += line.len();
    }
    if let Some((start, _)) = open {
        ranges.push(start..body.len());
    }
    ranges
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
    if !render.task_available {
        // Delegation is impossible without the `task` tool: run the skill
        // inline instead of telling the model to call a tool it lacks.
        let header_len = section.markdown.len() - body_after_frontmatter(&section.markdown).len();
        section.markdown = format!(
            "{}Note: this skill asks for context: fork, but the task tool is unavailable in this turn, so \
             it was not forked; follow it inline.\n\n{body}",
            &section.markdown[..header_len]
        );
        return;
    }
    let agent = match render.agent {
        Some(agent) => format!("agent_type \"{agent}\""),
        None => "no agent_type (the default explorer)".to_owned(),
    };
    // Claude-style values degrade instead of failing the fork: `inherit` means
    // "no override"; any other value the task tool refuses (a raw model id, an
    // unknown alias, an agent type that pins its own model) is retried once
    // without a model so the child uses the session or persona model.
    let model = match render.model {
        Some(model) if model.eq_ignore_ascii_case("inherit") => String::new(),
        Some(model) => format!(
            " and model \"{model}\" (the task tool only accepts a configured quick-model alias; \
             if it rejects the model argument, call task once more with the same agent_type and prompt \
             and no model so the child uses its default model, and add a one-line note to your \
             reply that the skill's model was not applied)"
        ),
        None => String::new(),
    };
    let once = if render
        .model
        .is_some_and(|m| !m.eq_ignore_ascii_case("inherit"))
    {
        "Call the task tool once"
    } else {
        "Call the task tool exactly once"
    };
    section.markdown = format!(
        "This skill is configured with context: fork. Do not follow its instructions yourself. \
         {once} with {agent}{model} and a single prompt that is exactly the \
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
                task_available: true,
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
    fn fork_without_task_tool_renders_inline_with_a_notice() {
        let mut section = AgentSection {
            digest: "d".into(),
            markdown: "---\nname: s\ncontext: fork\n---\nReview $ARGUMENTS now\n".into(),
            resources: vec![("r.md".into(), 1, "x".into(), None)],
            #[cfg(feature = "skills")]
            learned_js: Vec::new(),
            score: 1.0,
            rank: 0,
        };
        render_section(
            &mut section,
            &PinRender {
                arguments: "a.rs",
                skill_dir: "/d",
                fork: true,
                task_available: false,
                agent: Some("reviewer"),
                model: None,
            },
        );
        assert!(section.markdown.contains("task tool is unavailable"));
        assert!(section.markdown.contains("Review a.rs now"));
        assert!(!section.markdown.contains("Call the task tool"));
        assert!(!section.resources.is_empty());
    }

    #[test]
    fn positional_tokens_need_arguments_and_skip_fences_and_honour_escape() {
        let awk = "Run:\n```sh\nawk '{print $1}' f\n```\ncosts $5 and $$1 and $$ARGUMENTS";
        // No arguments: nothing positional is rewritten.
        let none = substitute(awk, "", "/d");
        assert!(none.contains("awk '{print $1}' f"), "{none}");
        assert!(none.contains("costs $5 and $1 and $ARGUMENTS"), "{none}");
        // With arguments: prose `$N` substitutes, the fenced one survives.
        let some = substitute(awk, "x y", "/d");
        assert!(some.contains("awk '{print $1}' f"), "{some}");
        assert!(some.contains("costs  and $1 and $ARGUMENTS"), "{some}");
        // `$ARGUMENTS` inside a fence still substitutes.
        assert!(substitute("```\n$ARGUMENTS\n```", "q", "/d").contains("\nq\n"));
    }

    #[test]
    fn paths_scope_selection_to_requests_naming_a_matching_file() {
        let root = Path::new("/work/proj");
        let m = |q: &str| paths_match_request(&scoped(), q, root, &[]);
        fn scoped() -> Vec<String> {
            vec!["src/**/*.rs".to_owned(), "docs/*.md".to_owned()]
        }
        assert!(paths_match_request(&[], "anything", root, &[]));
        assert!(m("please fix `src/a/b.rs`."));
        assert!(m("edit ./docs/x.md now"));
        // @mention, absolute under the workspace, and a bare filename.
        assert!(m("look at @src/a.rs"));
        assert!(m("open /work/proj/src/a/b.rs please"));
        assert!(m("fix the bug in lib.rs"));
        // Non-matching sessions stay excluded.
        assert!(!m("fix the bug in the parser"));
        assert!(!m("tests/a.rs"));
        assert!(!m("/other/proj/src/a.rs"));
        assert!(!m("notes.txt"));
        // A traversal component never matches a scope, but `..` inside a name is fine.
        assert!(!m("src/../../etc/passwd.rs"));
        assert!(m("src/a..b.rs"));
        // A path beyond the 8 KiB retrieval cut still counts.
        let long = format!("{} src/late.rs", "word ".repeat(4000));
        assert!(m(&long));
        // Catch-all patterns do not match arbitrary bare words.
        let wide = vec!["src/**".to_owned()];
        assert!(!paths_match_request(&wide, "update lib.rs", root, &[]));
        // A file the agent touched earlier activates the skill without being named.
        let touched = vec!["src/deep/mod.rs".to_owned()];
        assert!(paths_match_request(&scoped(), "continue", root, &touched));
        assert!(!paths_match_request(
            &scoped(),
            "continue",
            root,
            &["README.md".to_owned()]
        ));
    }

    #[test]
    fn fork_model_values_from_claude_degrade_instead_of_failing() {
        let render = |model: &str| {
            let mut section = AgentSection {
                digest: "d".into(),
                markdown: "---\nname: s\ncontext: fork\n---\nGo\n".into(),
                resources: Vec::new(),
                #[cfg(feature = "skills")]
                learned_js: Vec::new(),
                score: 1.0,
                rank: 0,
            };
            render_section(
                &mut section,
                &PinRender {
                    arguments: "",
                    skill_dir: "/d",
                    fork: true,
                    task_available: true,
                    agent: None,
                    model: Some(model),
                },
            );
            section.markdown
        };
        // `inherit` passes no model at all.
        let inherit = render("Inherit");
        assert!(!inherit.contains("model \""), "{inherit}");
        assert!(inherit.contains("exactly once"), "{inherit}");
        // A raw id / unknown alias is tried, then retried once without it.
        for value in ["opus", "claude-sonnet-4-5"] {
            let text = render(value);
            assert!(text.contains(&format!("model \"{value}\"")), "{text}");
            assert!(text.contains("and no model"), "{text}");
            assert!(!text.contains("instead of retrying"), "{text}");
        }
    }
}
