//! Command names learned in other tools. `/resume` and friends dispatch to
//! their mini-agent equivalent, hint-only names say what to use instead, and
//! any other unknown name gets the closest built-in or custom command.
//!
//! These are deliberately not reserved: a custom command with the same name
//! wins, and they are not offered by completion (that list stays canonical).

/// What a foreign command name maps to.
pub(super) enum Foreign {
    /// Run this command instead, keeping the arguments after the name.
    Rewrite(&'static str),
    /// Same, but only without arguments; with arguments print the hint.
    RewriteBare(&'static str, &'static str),
    /// No equivalent: print this.
    Hint(&'static str),
}

pub(super) fn foreign(name: &str) -> Option<Foreign> {
    Some(match name {
        "/resume" => Foreign::Rewrite("/sessions"),
        "/status" => Foreign::Rewrite("/context"),
        "/usage" => Foreign::Rewrite("/cost"),
        "/agents" => Foreign::Rewrite("/agent"),
        "/todos" => Foreign::RewriteBare(
            "/toggle todo",
            "use /toggle todo to show or hide the todo list",
        ),
        "/plan" => Foreign::RewriteBare(
            "/mode planwrite",
            "/plan takes no description here: run /mode planwrite, then send your request",
        ),
        "/statusline" => Foreign::Hint(
            "/statusline: there is no status line editor; see /config for footer settings",
        ),
        "/release-notes" => Foreign::Hint("/release-notes: see CHANGELOG.md in the project"),
        "/bug" => Foreign::Hint("/bug: use /doctor to collect diagnostics for a report"),
        _ => return None,
    })
}

/// The text to dispatch instead of `text`, when its command is a foreign
/// alias with a true equivalent. The caller checks custom commands first.
pub(super) fn rewrite(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let (name, rest) = trimmed
        .split_once(char::is_whitespace)
        .map_or((trimmed, ""), |(name, rest)| (name, rest.trim()));
    match foreign(name)? {
        Foreign::Rewrite(target) => Some(if rest.is_empty() {
            target.to_owned()
        } else {
            format!("{target} {rest}")
        }),
        Foreign::RewriteBare(target, _) if rest.is_empty() => Some(target.to_owned()),
        _ => None,
    }
}

/// The hint for a foreign name that has no (bare) equivalent.
pub(super) fn hint(name: &str) -> Option<&'static str> {
    match foreign(name)? {
        Foreign::Hint(text) | Foreign::RewriteBare(_, text) => Some(text),
        Foreign::Rewrite(_) => None,
    }
}

/// Up to three commands close to the mistyped `name` (with its slash).
pub(super) fn suggest<'a>(
    name: &str,
    builtins: &[&'a str],
    custom: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let wanted = name.to_ascii_lowercase();
    let custom: Vec<String> = custom.map(|c| format!("/{c}")).collect();
    let mut scored: Vec<(usize, String)> = builtins
        .iter()
        .map(|c| (*c).to_owned())
        .chain(custom)
        .filter_map(|candidate| {
            let lower = candidate.to_ascii_lowercase();
            let distance = crate::config::keys::edit_distance(&wanted, &lower);
            let near = distance <= 2.max(wanted.len() / 4) || lower.starts_with(&wanted);
            near.then_some((distance, candidate))
        })
        .collect();
    scored.sort();
    scored.dedup_by(|a, b| a.1 == b.1);
    scored.into_iter().take(3).map(|(_, name)| name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvnkx131_aliases_rewrite_hint_and_suggest() {
        assert_eq!(rewrite("/resume").as_deref(), Some("/sessions"));
        assert_eq!(rewrite("/resume --all").as_deref(), Some("/sessions --all"));
        assert_eq!(rewrite("/plan").as_deref(), Some("/mode planwrite"));
        assert_eq!(rewrite("/plan do x"), None);
        assert!(hint("/plan").unwrap().contains("/mode planwrite"));
        assert_eq!(rewrite("/sessions"), None);
        let builtins = crate::ui::pickers::list::suggestion_candidates();
        assert_eq!(
            suggest("/sesions", builtins, std::iter::empty())
                .first()
                .map(String::as_str),
            Some("/sessions")
        );
        let custom = ["deploy"];
        assert_eq!(
            suggest("/deploi", builtins, custom.into_iter())
                .first()
                .map(String::as_str),
            Some("/deploy")
        );
        assert!(suggest("/zzzzzzzzzz", builtins, std::iter::empty()).is_empty());
    }
}
