//! Claude Code style project rules: `.claude/rules/**/*.md`.
//!
//! A rule is a markdown file whose optional YAML frontmatter may carry a
//! `paths:` list of globs. A rule without `paths` is unconditional and loads
//! with the other root instruction files at session start. A rule with `paths`
//! is delivered the first time a tool touches a file matching one of its
//! globs, through the same nested-instruction mechanism (and trust, read
//! authority, byte budget and data framing) as a nested `CLAUDE.md`.
//!
//! Frontmatter keys other than `paths` are ignored, and frontmatter that does
//! not parse never fails the load: the rule is then unconditional.
use std::path::Path;

/// Where project rules live, relative to the workspace root.
pub(crate) const RULES_DIR: &str = ".claude/rules";
/// Rule files considered per workspace.
pub(crate) const MAX_RULE_FILES: usize = 64;
/// Directory levels below [`RULES_DIR`] that are searched.
pub(crate) const MAX_RULE_DEPTH: usize = 4;
/// Directory entries scanned while looking for rules.
pub(crate) const MAX_RULE_ENTRIES: usize = 1024;
/// Globs honoured per rule.
const MAX_PATTERNS: usize = 32;
/// Longest accepted glob.
const MAX_PATTERN_BYTES: usize = 512;
/// Largest frontmatter block parsed.
const MAX_FRONTMATTER_BYTES: usize = 4096;

/// Whether `path` (workspace-relative) is a project rule file.
pub(crate) fn is_rule_path(path: &Path) -> bool {
    path.starts_with(RULES_DIR)
}

/// Splits a leading `---` frontmatter block from `text`:
/// `(frontmatter_yaml, body)`. Text without a well-formed, terminated block
/// is all body.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (None, text);
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            if offset > MAX_FRONTMATTER_BYTES {
                return (None, text);
            }
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
        if offset > MAX_FRONTMATTER_BYTES {
            return (None, text);
        }
    }
    (None, text)
}

/// `text` without its frontmatter block.
pub(crate) fn strip_frontmatter(text: &str) -> String {
    split_frontmatter(text).1.to_owned()
}

/// A rule split into its path scope and its authored body.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedRule {
    /// `None`: unconditional. `Some`: the non-empty glob list that scopes it.
    pub(crate) patterns: Option<Vec<String>>,
    /// The rule text without frontmatter.
    pub(crate) body: String,
}

/// Parses one rule file. Never fails: anything unusable in the frontmatter
/// leaves the rule unconditional with its body intact.
pub(crate) fn parse_rule(text: &str) -> ParsedRule {
    let (frontmatter, body) = split_frontmatter(text);
    let patterns = frontmatter.and_then(frontmatter_patterns);
    ParsedRule {
        patterns,
        body: body.to_owned(),
    }
}

fn frontmatter_patterns(yaml: &str) -> Option<Vec<String>> {
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).ok()?;
    let paths = value.as_mapping()?.get("paths")?;
    let mut patterns: Vec<String> = match paths {
        serde_yaml_ng::Value::String(text) => split_patterns(text),
        serde_yaml_ng::Value::Sequence(items) => items
            .iter()
            .filter_map(serde_yaml_ng::Value::as_str)
            .flat_map(split_patterns)
            .collect(),
        _ => return None,
    };
    patterns.retain(|pattern| !pattern.is_empty() && pattern.len() <= MAX_PATTERN_BYTES);
    patterns.truncate(MAX_PATTERNS);
    // Only globs that compile scope a rule; a rule whose globs are all
    // unusable is unconditional rather than silently never applying.
    patterns.retain(|pattern| compile_glob(pattern).is_some());
    (!patterns.is_empty()).then_some(patterns)
}

/// A scalar `paths` value may list several globs separated by commas, but a
/// comma inside `{a,b}` braces belongs to the glob.
fn split_patterns(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for character in text.chars() {
        match character {
            '{' => {
                depth += 1;
                current.push(character);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(character),
        }
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|part| part.trim().trim_matches(['"', '\'']).trim().to_owned())
        .collect()
}

fn compile_glob(pattern: &str) -> Option<globset::Glob> {
    globset::GlobBuilder::new(pattern.trim_start_matches("./"))
        .literal_separator(true)
        .build()
        .ok()
}

/// The compiled globs of one scoped rule.
pub(crate) fn compile(patterns: &[String]) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(compile_glob(pattern)?);
    }
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_without_frontmatter_is_unconditional() {
        let rule = parse_rule("Use tabs.\n");
        assert_eq!(rule.patterns, None);
        assert_eq!(rule.body, "Use tabs.\n");
    }

    #[test]
    fn paths_list_scopes_a_rule_and_other_keys_are_ignored() {
        let rule = parse_rule(
            "---\ndescription: API rules\npaths:\n  - \"src/api/**/*.ts\"\n  - 'lib/*.rs'\nunknown: [1, 2]\n---\nBody\n",
        );
        assert_eq!(
            rule.patterns,
            Some(vec!["src/api/**/*.ts".to_owned(), "lib/*.rs".to_owned()])
        );
        assert_eq!(rule.body, "Body\n");
    }

    #[test]
    fn scalar_and_inline_forms_keep_brace_commas() {
        let rule = parse_rule("---\npaths: src/**/*.{ts,tsx}, docs/**\n---\nB");
        assert_eq!(
            rule.patterns,
            Some(vec!["src/**/*.{ts,tsx}".to_owned(), "docs/**".to_owned()])
        );
        let inline = parse_rule("---\npaths: [\"a/**\", \"b/*.md\"]\n---\nB");
        assert_eq!(
            inline.patterns,
            Some(vec!["a/**".to_owned(), "b/*.md".to_owned()])
        );
    }

    #[test]
    fn unusable_frontmatter_never_fails_and_leaves_the_rule_unconditional() {
        for text in [
            "---\npaths: [unterminated\n---\nBody",
            "---\npaths: 12\n---\nBody",
            "---\npaths: []\n---\nBody",
            "---\npaths: \"[\"\n---\nBody",
            "---\nnot: yaml: at: all:\n---\nBody",
            "---\nno terminator\nBody",
        ] {
            assert_eq!(parse_rule(text).patterns, None, "{text:?}");
        }
        // An unterminated block is body text, kept verbatim.
        assert_eq!(
            parse_rule("---\nno terminator\nBody").body,
            "---\nno terminator\nBody"
        );
    }

    #[test]
    fn oversized_frontmatter_is_not_parsed() {
        let text = format!("---\npaths: [\"a/**\"]\nx: {}\n---\nBody", "y".repeat(8192));
        let rule = parse_rule(&text);
        assert_eq!(rule.patterns, None);
        assert_eq!(rule.body, text);
    }

    #[test]
    fn globs_match_workspace_relative_paths_with_literal_separators() {
        let set = compile(&["src/**/*.rs".to_owned(), "*.md".to_owned()]).unwrap();
        assert!(set.is_match(Path::new("src/a/b.rs")));
        assert!(set.is_match(Path::new("README.md")));
        assert!(!set.is_match(Path::new("docs/README.md")));
        assert!(!set.is_match(Path::new("other/b.rs")));
    }

    #[test]
    fn pattern_count_and_length_are_bounded() {
        let many = (0..100)
            .map(|index| format!("\"d{index}/**\""))
            .collect::<Vec<_>>()
            .join(", ");
        let rule = parse_rule(&format!("---\npaths: [{many}]\n---\nB"));
        assert_eq!(rule.patterns.unwrap().len(), MAX_PATTERNS);
        let long = "a".repeat(MAX_PATTERN_BYTES + 1);
        assert_eq!(
            parse_rule(&format!("---\npaths: [\"{long}\"]\n---\nB")).patterns,
            None
        );
    }
}
