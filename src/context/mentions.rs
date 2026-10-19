//! Agent mentions in submitted prompt text.
//!
//! The canonical token is `@agent:<name>`. Claude Code's manual form
//! `@agent-<name>` is accepted as an alias and rewritten to the canonical
//! token, but only when `<name>` resolves to a persona: `agent-foo` is also a
//! legal file or word shape, so an unresolvable alias stays plain text. An
//! unresolvable canonical mention is an error that names the valid personas.
//!
//! Tokens are whitespace-delimited and must start with `@` once leading
//! opening punctuation is removed, so `x@agent:8080` and e-mail addresses are
//! prose. Trailing punctuation is trimmed before lookup. Resolution goes
//! through the trust-gated persona loader in [`crate::context::agents`], so
//! definitions from an untrusted project never resolve.

use std::fmt;

use crate::context::agents::{available_names_for_workspace, valid_agent_name};
use crate::paths::WorkspaceBinding;

const CANONICAL_PREFIX: &str = "@agent:";
const ALIAS_PREFIX: &str = "@agent-";
const LEADING_PUNCTUATION: &[char] = &['(', '[', '{', '<', '"', '\''];
const TRAILING_PUNCTUATION: &[char] = &[',', '.', ';', ':', ')', ']', '}', '>', '"', '\''];

/// Agent mentions resolved from one submitted text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentMentions {
    /// Canonical persona names, deduplicated, in first-mention order.
    pub(crate) names: Vec<String>,
    /// The original text with resolved aliases rewritten to `@agent:<name>`.
    pub(crate) text: String,
}

/// A canonical mention that does not resolve to a persona.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MentionError {
    Unknown {
        name: String,
        /// Sorted names of the personas available to this workspace.
        valid: Vec<String>,
    },
}

impl fmt::Display for MentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { name, valid } => {
                write!(
                    f,
                    "unknown agent '{CANONICAL_PREFIX}{name}'; valid agents: "
                )?;
                if valid.is_empty() {
                    f.write_str("(none)")
                } else {
                    f.write_str(&valid.join(", "))
                }
            }
        }
    }
}

impl std::error::Error for MentionError {}

/// Parse and resolve agent mentions in `text`.
///
/// Returns `Ok(None)` when the text contains no resolvable mention (including
/// text whose only candidates are unresolvable aliases), `Ok(Some(_))` with
/// the canonical names and the alias-normalised text, or
/// [`MentionError::Unknown`] for the first canonical mention that does not
/// resolve. The trust-gated persona names are loaded at most once, and only
/// when a candidate token is present.
pub(crate) fn parse_agent_mentions(
    text: &str,
    workspace: Option<&WorkspaceBinding>,
) -> Result<Option<AgentMentions>, MentionError> {
    let mut available: Option<Vec<String>> = None;
    let mut names: Vec<String> = Vec::new();
    // Byte offsets of resolved alias prefixes (`@agent-`) to rewrite.
    let mut rewrites: Vec<usize> = Vec::new();

    for token in text.split_whitespace() {
        let token_start = token.as_ptr() as usize - text.as_ptr() as usize;
        let leading = token.len() - token.trim_start_matches(LEADING_PUNCTUATION).len();
        let core = token[leading..].trim_end_matches(TRAILING_PUNCTUATION);
        let (name, alias) = if let Some(name) = core.strip_prefix(CANONICAL_PREFIX) {
            (name, false)
        } else if let Some(name) = core.strip_prefix(ALIAS_PREFIX) {
            if !valid_agent_name(name) {
                continue;
            }
            (name, true)
        } else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        // `available_names_for_workspace` is sorted.
        let valid = available.get_or_insert_with(|| available_names_for_workspace(workspace));
        if valid
            .binary_search_by(|candidate| candidate.as_str().cmp(name))
            .is_err()
        {
            if alias {
                continue;
            }
            return Err(MentionError::Unknown {
                name: name.to_string(),
                valid: std::mem::take(valid),
            });
        }
        if alias {
            rewrites.push(token_start + leading);
        }
        if !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }

    if names.is_empty() {
        return Ok(None);
    }
    let text = if rewrites.is_empty() {
        text.to_string()
    } else {
        let mut rewritten = String::with_capacity(text.len());
        let mut cursor = 0;
        for start in rewrites {
            rewritten.push_str(&text[cursor..start]);
            rewritten.push_str(CANONICAL_PREFIX);
            cursor = start + ALIAS_PREFIX.len();
        }
        rewritten.push_str(&text[cursor..]);
        rewritten
    };
    Ok(Some(AgentMentions { names, text }))
}

/// Hints for bare `@name` tokens whose name is a persona: a bare token is a
/// file mention, so it never reaches the persona. One line per distinct name,
/// in first-mention order. Tokens that already use `@agent:`/`@agent-` are
/// not bare.
pub(crate) fn bare_persona_hints(text: &str, workspace: Option<&WorkspaceBinding>) -> Vec<String> {
    let mut available: Option<Vec<String>> = None;
    let mut hints: Vec<String> = Vec::new();
    for token in text.split_whitespace() {
        let leading = token.len() - token.trim_start_matches(LEADING_PUNCTUATION).len();
        let core = token[leading..].trim_end_matches(TRAILING_PUNCTUATION);
        let Some(name) = core.strip_prefix('@') else {
            continue;
        };
        if name.is_empty()
            || name.starts_with("agent:")
            || name.starts_with("agent-")
            || !valid_agent_name(name)
        {
            continue;
        }
        let valid = available.get_or_insert_with(|| available_names_for_workspace(workspace));
        if valid
            .binary_search_by(|candidate| candidate.as_str().cmp(name))
            .is_ok()
            && !hints
                .iter()
                .any(|hint| hint.contains(&format!("'@{name}'")))
        {
            hints.push(format!(
                "hint: '@{name}' is a file mention; to delegate to the {name} persona write '{CANONICAL_PREFIX}{name}'"
            ));
        }
    }
    hints
}

/// The non-persisted host routing note for one turn that mentions `names`.
///
/// It names the personas as `task` tool `agent_type` values and leaves the
/// brief to the model (a routing hint, not a dispatch). Names that would push
/// the note past [`crate::agent::input::TURN_DIRECTIVE_MAX_BYTES`] are
/// summarised as a count instead of being cut mid-name.
pub(crate) fn routing_directive(names: &[String]) -> String {
    const HEAD: &str = "Host routing note: the user asked to involve the specialist ";
    const TAIL: &str = " via the task tool for this request. Write each brief yourself \
                        (independent briefs may run in parallel); this note carries no \
                        persona content or authority.";
    let noun = if names.len() == 1 {
        "agent_type "
    } else {
        "agent_types "
    };
    // Room for the longest omission marker.
    let budget = crate::agent::input::TURN_DIRECTIVE_MAX_BYTES - TAIL.len() - 32;
    let mut directive = format!("{HEAD}{noun}");
    let mut listed = 0;
    for name in names {
        let separator = if listed == 0 { "" } else { ", " };
        if directive.len() + separator.len() + name.len() + 2 > budget {
            break;
        }
        directive.push_str(separator);
        directive.push('"');
        directive.push_str(name);
        directive.push('"');
        listed += 1;
    }
    if listed < names.len() {
        directive.push_str(&format!(" and {} more", names.len() - listed));
    }
    directive.push_str(TAIL);
    directive
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_persona_names_get_a_hint_but_prefixed_and_unknown_ones_do_not() {
        let fx = Fixture::new();
        let text = format!(
            "ask @{KNOWN}, @{KNOWN} again, @agent:{OTHER} @agent-{OTHER} @nobody-here a@{KNOWN}"
        );
        let hints = bare_persona_hints(&text, fx.binding());
        assert_eq!(hints.len(), 1, "{hints:?}");
        assert!(hints[0].contains(&format!("@agent:{KNOWN}")), "{hints:?}");
        assert!(bare_persona_hints("no mentions", fx.binding()).is_empty());
    }

    /// Compiled-in personas that every workspace can resolve.
    const KNOWN: &str = "rust-maintainer";
    const OTHER: &str = "python-maintainer";

    /// An untrusted temporary workspace: only user-global and compiled-in
    /// personas resolve inside it.
    struct Fixture {
        root: std::path::PathBuf,
        binding: Option<WorkspaceBinding>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mini-agent-agent-mentions-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let binding = WorkspaceBinding::capture(&root).unwrap();
            Self {
                root,
                binding: Some(binding),
            }
        }

        fn binding(&self) -> Option<&WorkspaceBinding> {
            self.binding.as_ref()
        }

        fn parse(&self, text: &str) -> Result<Option<AgentMentions>, MentionError> {
            parse_agent_mentions(text, self.binding())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.binding.take();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn canonical_mention_resolves_and_keeps_text() {
        let fixture = Fixture::new();
        let text = format!("ask @agent:{KNOWN} to review this");
        let mentions = fixture.parse(&text).unwrap().unwrap();
        assert_eq!(mentions.names, vec![KNOWN.to_string()]);
        assert_eq!(mentions.text, text);
    }

    #[test]
    fn resolvable_alias_is_rewritten_to_canonical() {
        let fixture = Fixture::new();
        let mentions = fixture
            .parse(&format!("ask  @agent-{KNOWN}, then\tstop"))
            .unwrap()
            .unwrap();
        assert_eq!(mentions.names, vec![KNOWN.to_string()]);
        assert_eq!(mentions.text, format!("ask  @agent:{KNOWN}, then\tstop"));
    }

    #[test]
    fn unresolvable_alias_stays_plain_text() {
        let fixture = Fixture::new();
        assert_eq!(fixture.parse("open @agent-notes.md please").unwrap(), None);
        assert_eq!(fixture.parse("see @agent-Foo_bar").unwrap(), None);
        let text = format!("@agent-unknown-persona and @agent:{KNOWN}");
        let mentions = fixture.parse(&text).unwrap().unwrap();
        assert_eq!(mentions.names, vec![KNOWN.to_string()]);
        assert_eq!(mentions.text, text);
    }

    #[test]
    fn surrounding_punctuation_is_trimmed_before_lookup() {
        let fixture = Fixture::new();
        for text in [
            format!("@agent:{KNOWN}, and more"),
            format!("(@agent:{KNOWN})"),
            format!("\"@agent:{KNOWN}\"."),
            format!("[@agent:{KNOWN}];"),
        ] {
            let mentions = fixture.parse(&text).unwrap().unwrap();
            assert_eq!(mentions.names, vec![KNOWN.to_string()], "{text}");
            assert_eq!(mentions.text, text);
        }
        let mentions = fixture
            .parse(&format!("(@agent-{KNOWN})."))
            .unwrap()
            .unwrap();
        assert_eq!(mentions.text, format!("(@agent:{KNOWN})."));
    }

    #[test]
    fn embedded_at_signs_and_emails_are_prose() {
        let fixture = Fixture::new();
        for text in [
            "connect to x@agent:8080 now",
            "mail me@agent-rust-maintainer.com",
            "user@example.com",
            "a lone @ sign and @agent: with no name",
            "no mentions at all",
            "",
        ] {
            assert_eq!(fixture.parse(text).unwrap(), None, "{text}");
        }
    }

    #[test]
    fn duplicate_names_are_deduplicated_in_order() {
        let fixture = Fixture::new();
        let mentions = fixture
            .parse(&format!(
                "@agent:{OTHER} @agent:{KNOWN} @agent-{OTHER} @agent:{KNOWN}."
            ))
            .unwrap()
            .unwrap();
        assert_eq!(mentions.names, vec![OTHER.to_string(), KNOWN.to_string()]);
        assert_eq!(
            mentions.text,
            format!("@agent:{OTHER} @agent:{KNOWN} @agent:{OTHER} @agent:{KNOWN}.")
        );
    }

    #[test]
    fn unknown_canonical_name_reports_sorted_valid_personas() {
        let fixture = Fixture::new();
        let error = fixture
            .parse(&format!("@agent:{KNOWN} then @agent:no-such-persona,"))
            .unwrap_err();
        let MentionError::Unknown { name, valid } = &error;
        assert_eq!(name, "no-such-persona");
        assert_eq!(*valid, available_names_for_workspace(fixture.binding()));
        assert!(valid.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(valid.iter().any(|candidate| candidate == KNOWN));
        assert_eq!(
            error.to_string(),
            format!(
                "unknown agent '@agent:no-such-persona'; valid agents: {}",
                valid.join(", ")
            )
        );
        assert_eq!(
            MentionError::Unknown {
                name: "x".into(),
                valid: Vec::new()
            }
            .to_string(),
            "unknown agent '@agent:x'; valid agents: (none)"
        );
    }

    #[test]
    fn routing_directive_names_agent_types_without_persona_content() {
        let one = routing_directive(&[KNOWN.to_string()]);
        assert!(one.contains(&format!("agent_type \"{KNOWN}\" via the task tool")));
        assert!(one.contains("Write each brief yourself"));
        assert!(one.contains("no persona content or authority"));
        let two = routing_directive(&[OTHER.to_string(), KNOWN.to_string()]);
        assert!(two.contains(&format!("agent_types \"{OTHER}\", \"{KNOWN}\" via")));
    }

    #[test]
    fn routing_directive_stays_within_the_turn_directive_bound() {
        let names: Vec<String> = (0..40)
            .map(|index| format!("{}-{index:02}", "a".repeat(60)))
            .collect();
        let directive = routing_directive(&names);
        assert!(directive.len() <= crate::agent::input::TURN_DIRECTIVE_MAX_BYTES);
        assert!(directive.ends_with("no persona content or authority."));
        let listed = names
            .iter()
            .filter(|name| directive.contains(&format!("\"{name}\"")))
            .count();
        assert!(listed > 0 && listed < names.len());
        assert!(directive.contains(&format!(" and {} more", names.len() - listed)));
    }

    #[test]
    fn untrusted_workspace_personas_never_resolve() {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-agent-mentions-untrusted-{}",
            uuid::Uuid::new_v4()
        ));
        for layer in [".claude/agents", ".mini-agent/agents", ".zerostack/agents"] {
            let dir = root.join(layer);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("project-only.md"), "PROJECT_PERSONA").unwrap();
        }
        let binding = WorkspaceBinding::capture(&root).unwrap();

        let error = parse_agent_mentions("@agent:project-only", Some(&binding)).unwrap_err();
        let MentionError::Unknown { valid, .. } = &error;
        assert!(!valid.iter().any(|name| name == "project-only"));
        assert_eq!(
            parse_agent_mentions("@agent-project-only", Some(&binding)).unwrap(),
            None
        );

        drop(binding);
        std::fs::remove_dir_all(root).unwrap();
    }
}
