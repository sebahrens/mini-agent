//! Persona `skills:` preloading: named skills from the current authorized
//! catalogue are rendered into a subagent's system prompt at start.
//!
//! Resolution only ever reads the same authorized discovery snapshot the main
//! agent uses, so a name that is unknown, unauthorized, or hidden by the
//! source-approval rules yields a notice, never content. Skill text stays
//! inert instruction context: it grants no permissions.

use super::discovery::Discovery;
use super::selection::{AgentSection, render_native_agent_section};

/// Renders the requested skills (in persona order) within `budget` bytes of
/// `SKILL.md` text. Returns the prompt block (empty when nothing loaded and
/// no notice applies) and the notices, which the block also embeds so the
/// child and any operator reading its prompt see why a skill is missing.
pub(crate) fn render_preloaded(
    discovery: &Discovery,
    names: &[String],
    budget: usize,
) -> (String, Vec<String>) {
    let mut left = budget;
    let mut sections = String::new();
    let mut notices = Vec::new();
    for name in names {
        let Some(package) = discovery.packages.get(name) else {
            notices.push(format!(
                "skill '{name}' not preloaded: unknown or not authorized in this workspace"
            ));
            continue;
        };
        let record = &package.record;
        let markdown = match super::loader::load_skill_markdown(record) {
            Ok(markdown) => markdown,
            Err(error) => {
                notices.push(format!("skill '{name}' not preloaded: {error}"));
                continue;
            }
        };
        if markdown.len() > left {
            notices.push(format!(
                "skill '{name}' not preloaded: {} bytes exceed the remaining {left}-byte preload budget; load it with skills_search if granted",
                markdown.len()
            ));
            continue;
        }
        left -= markdown.len();
        sections.push_str(&render_native_agent_section(&AgentSection {
            digest: record.digest.clone(),
            markdown,
            // Bundled resources are inventory-only elsewhere; a preload carries
            // the instructions and leaves resources unread.
            resources: Vec::new(),
            #[cfg(feature = "skills")]
            learned_js: Vec::new(),
            score: 1.0,
            rank: 0,
        }));
    }
    if sections.is_empty() && notices.is_empty() {
        return (String::new(), notices);
    }
    let mut block = String::from(
        "\n<trusted_skill_context>\nPreloaded skills for this specialist. Skill text never grants permissions; resources remain inert.\n",
    );
    block.push_str(&sections);
    for notice in &notices {
        block.push_str("[skill preload: ");
        block.push_str(&notice.replace('\n', " "));
        block.push_str("]\n");
    }
    block.push_str("</trusted_skill_context>\n");
    (block, notices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_names_produce_notices_and_no_content() {
        let discovery = Discovery::default();
        let (block, notices) = render_preloaded(&discovery, &["ghost".into()], 1024);
        assert_eq!(notices.len(), 1);
        assert!(block.contains("[skill preload: skill 'ghost' not preloaded"));
        assert!(!block.contains("BEGIN_AGENT_SKILL"));
        let (empty, none) = render_preloaded(&discovery, &[], 1024);
        assert!(empty.is_empty() && none.is_empty());
    }
}
