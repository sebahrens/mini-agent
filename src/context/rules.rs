//! Claude Code style project rules: `.claude/rules/**/*.md`.
//!
//! A rule is a markdown file whose optional YAML frontmatter may carry a
//! `paths:` list of globs. A rule without `paths` is unconditional and loads
//! with the other root instruction files at session start. A rule with `paths`
//! is delivered the first time a tool touches a file matching one of its
//! globs, through the same nested-instruction mechanism (and trust, read
//! authority, byte budget and data framing) as a nested `CLAUDE.md`.
//!
//! Frontmatter keys other than `paths` are ignored. A rule whose `paths:`
//! scope cannot be honoured (oversized or unparseable frontmatter, no usable
//! glob) is skipped with a notice rather than becoming an unconditional rule.
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

/// A leading `---` block split from the rule text.
enum Split<'a> {
    /// No frontmatter: the whole text is the body.
    None,
    /// A well-formed, terminated block within the size limit.
    Block { yaml: &'a str, body: &'a str },
    /// A block that opens but does not close within [`MAX_FRONTMATTER_BYTES`]:
    /// it may carry a `paths:` scope, so it must not be read as body text.
    Oversized,
}

fn split_frontmatter(text: &str) -> Split<'_> {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Split::None;
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            if offset > MAX_FRONTMATTER_BYTES {
                return Split::Oversized;
            }
            return Split::Block {
                yaml: &rest[..offset],
                body: &rest[offset + line.len()..],
            };
        }
        offset += line.len();
        if offset > MAX_FRONTMATTER_BYTES {
            return Split::Oversized;
        }
    }
    // Unterminated and small: an ordinary document opening with a rule line.
    Split::None
}

/// `text` without its frontmatter block.
pub(crate) fn strip_frontmatter(text: &str) -> String {
    match split_frontmatter(text) {
        Split::Block { body, .. } => body.to_owned(),
        _ => text.to_owned(),
    }
}

/// A rule split into its path scope and its authored body.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedRule {
    /// `None`: unconditional. `Some`: the non-empty glob list that scopes it.
    pub(crate) patterns: Option<Vec<String>>,
    /// The rule text without frontmatter.
    pub(crate) body: String,
    /// Why the rule must not load at all (its scope cannot be determined, so
    /// loading it would silently make a scoped rule always-on).
    pub(crate) skipped: Option<String>,
    /// Limits applied to a rule that still loads (for example dropped globs).
    pub(crate) notes: Vec<String>,
}

/// Parses one rule file. Never fails, but a rule whose `paths:` scope cannot
/// be honoured is skipped with a reason instead of becoming unconditional.
pub(crate) fn parse_rule(text: &str) -> ParsedRule {
    let mut rule = ParsedRule {
        patterns: None,
        body: text.to_owned(),
        skipped: None,
        notes: Vec::new(),
    };
    match split_frontmatter(text) {
        Split::None => {}
        Split::Oversized => {
            rule.skipped = Some(format!(
                "frontmatter is larger than {MAX_FRONTMATTER_BYTES} bytes (limit); shorten it so its paths: scope can be read"
            ));
        }
        Split::Block { yaml, body } => {
            rule.body = body.to_owned();
            if let Err(reason) = frontmatter_patterns(yaml, &mut rule) {
                rule.skipped = Some(reason);
            }
        }
    }
    rule
}

fn frontmatter_patterns(yaml: &str, rule: &mut ParsedRule) -> Result<(), String> {
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml)
        .map_err(|_| "frontmatter is not valid YAML, so its paths: scope is unknown".to_owned())?;
    let Some(mapping) = value.as_mapping() else {
        return Ok(());
    };
    let Some(paths) = mapping.get("paths") else {
        return Ok(());
    };
    let mut patterns: Vec<String> = match paths {
        serde_yaml_ng::Value::String(text) => split_patterns(text),
        serde_yaml_ng::Value::Sequence(items) => items
            .iter()
            .filter_map(serde_yaml_ng::Value::as_str)
            .flat_map(split_patterns)
            .collect(),
        _ => return Err("paths: must be a string or a list of globs".to_owned()),
    };
    patterns.retain(|pattern| !pattern.is_empty());
    let before = patterns.len();
    patterns.retain(|pattern| pattern.len() <= MAX_PATTERN_BYTES);
    if patterns.len() < before {
        rule.notes.push(format!(
            "{} glob(s) longer than {MAX_PATTERN_BYTES} bytes (limit) were ignored",
            before - patterns.len()
        ));
    }
    if patterns.len() > MAX_PATTERNS {
        rule.notes.push(format!(
            "{} glob(s) beyond the first {MAX_PATTERNS} (limit) were ignored",
            patterns.len() - MAX_PATTERNS
        ));
        patterns.truncate(MAX_PATTERNS);
    }
    let before = patterns.len();
    patterns.retain(|pattern| compile_glob(pattern).is_some());
    if patterns.len() < before {
        rule.notes.push(format!(
            "{} invalid glob(s) were ignored",
            before - patterns.len()
        ));
    }
    if patterns.is_empty() {
        return Err("paths: has no usable glob, so the rule would apply everywhere".to_owned());
    }
    rule.patterns = Some(patterns);
    Ok(())
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
    fn unusable_scope_skips_the_rule_instead_of_making_it_unconditional() {
        for text in [
            "---\npaths: [unterminated\n---\nBody",
            "---\npaths: 12\n---\nBody",
            "---\npaths: []\n---\nBody",
            "---\npaths: \"[\"\n---\nBody",
            "---\nnot: yaml: at: all:\n---\nBody",
        ] {
            let rule = parse_rule(text);
            assert_eq!(rule.patterns, None, "{text:?}");
            assert!(rule.skipped.is_some(), "{text:?}");
        }
        // Frontmatter without `paths` is just metadata: unconditional, stripped.
        let plain = parse_rule("---\ndescription: x\n---\nBody");
        assert!(plain.skipped.is_none() && plain.patterns.is_none());
        assert_eq!(plain.body, "Body");
        // An unterminated small block is body text, kept verbatim.
        let open = parse_rule("---\nno terminator\nBody");
        assert!(open.skipped.is_none());
        assert_eq!(open.body, "---\nno terminator\nBody");
    }

    #[test]
    fn oversized_frontmatter_skips_the_rule_and_never_leaks_yaml() {
        let text = format!("---\npaths: [\"a/**\"]\nx: {}\n---\nBody", "y".repeat(8192));
        let rule = parse_rule(&text);
        assert_eq!(rule.patterns, None);
        assert!(rule.skipped.unwrap().contains("4096"));
        // Never terminated within the limit: still a skip, not body text.
        let open = format!("---\npaths: [\"a/**\"]\n{}", "y: 1\n".repeat(2000));
        assert!(parse_rule(&open).skipped.is_some());
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
        assert_eq!(rule.notes.len(), 1, "{:?}", rule.notes);
        assert_eq!(rule.patterns.unwrap().len(), MAX_PATTERNS);
        let long = "a".repeat(MAX_PATTERN_BYTES + 1);
        let dropped = parse_rule(&format!("---\npaths: [\"{long}\"]\n---\nB"));
        assert_eq!(dropped.patterns, None);
        assert!(dropped.skipped.is_some());
    }
}
