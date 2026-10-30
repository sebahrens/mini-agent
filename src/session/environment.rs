//! The execution environment a session last ran in.
//!
//! A resumed session replays conclusions the model drew under the old
//! environment ("no network", "~/.cargo is empty"). When the sandbox, permission
//! mode, workspace or tool set differs on resume those conclusions are stale and
//! the model must be told, once (mini-agent-1pi7h).

use serde::{Deserialize, Serialize};

/// What the model can reach and do, as a few comparable labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentRecord {
    /// Effective sandbox: `off` or `<backend>/<profile>`.
    pub sandbox: String,
    pub permission_mode: String,
    pub workspace: String,
    pub tools: String,
}

impl EnvironmentRecord {
    /// One `label: was -> now` line per differing field.
    fn changes(&self, now: &Self) -> Vec<String> {
        [
            ("sandbox", &self.sandbox, &now.sandbox),
            (
                "permission mode",
                &self.permission_mode,
                &now.permission_mode,
            ),
            ("workspace", &self.workspace, &now.workspace),
            ("tools", &self.tools, &now.tools),
        ]
        .into_iter()
        .filter(|(_, was, is)| was != is)
        .map(|(label, was, is)| format!("- {label}: {was} -> {is}"))
        .collect()
    }
}

impl super::Session {
    /// Record `now` as this session's environment. When the session was
    /// `resumed` and the environment differs from the recorded one, append a
    /// single system notice naming the differences. Returns whether it did.
    /// A session with no record (new, or saved by an older build) only records.
    pub fn sync_environment(&mut self, now: EnvironmentRecord, resumed: bool) -> bool {
        let changes = match (&self.environment, resumed) {
            (Some(was), true) => Some(was.changes(&now)).filter(|c| !c.is_empty()),
            _ => None,
        };
        self.environment = Some(now);
        let Some(changes) = changes else {
            return false;
        };
        let text = format!(
            "The execution environment changed since this session was last active:\n{}\n\
             Earlier conclusions about the environment (network access, installed tools, \
             credentials, which files exist or are writable) may no longer hold; re-check \
             before relying on them or acting on them.",
            changes.join("\n")
        );
        self.add_message(super::MessageRole::System, &text);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{MessageRole, Session};

    fn env(sandbox: &str) -> EnvironmentRecord {
        EnvironmentRecord {
            sandbox: sandbox.into(),
            permission_mode: "standard".into(),
            workspace: "/w".into(),
            tools: "all".into(),
        }
    }

    fn session() -> Session {
        Session::new("p", "m", 1000, "s")
    }

    fn system_notices(s: &Session) -> usize {
        s.messages
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .count()
    }

    #[test]
    fn resume_with_a_different_sandbox_injects_exactly_one_notice() {
        let mut s = session();
        assert!(!s.sync_environment(env("bwrap/workspace"), false));
        assert!(s.sync_environment(env("off"), true));
        assert_eq!(system_notices(&s), 1);
        let text = s.messages.last().unwrap().content.to_string();
        assert!(text.contains("- sandbox: bwrap/workspace -> off"), "{text}");
        assert!(!text.contains("permission mode"));
        // Resuming again in the same environment stays quiet.
        assert!(!s.sync_environment(env("off"), true));
        assert_eq!(system_notices(&s), 1);
    }

    #[test]
    fn identical_or_unrecorded_environments_inject_nothing() {
        let mut s = session();
        // Legacy session without a record: nothing to compare against.
        assert!(!s.sync_environment(env("off"), true));
        assert!(!s.sync_environment(env("off"), true));
        assert_eq!(system_notices(&s), 0);
    }

    #[test]
    fn each_differing_field_is_named() {
        let mut s = session();
        s.sync_environment(env("off"), false);
        let mut now = env("off");
        now.permission_mode = "yolo".into();
        now.workspace = "/other".into();
        now.tools = "read_file".into();
        assert!(s.sync_environment(now, true));
        let text = s.messages.last().unwrap().content.to_string();
        for needle in [
            "permission mode: standard -> yolo",
            "workspace: /w -> /other",
            "tools: all -> read_file",
        ] {
            assert!(text.contains(needle), "{text}");
        }
    }

    #[test]
    fn the_record_round_trips_through_the_session_file() {
        let mut s = session();
        s.sync_environment(env("off"), false);
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.environment, s.environment);
    }
}
