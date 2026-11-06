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

/// Start of the system notice [`Session::note_stale_child_ids`] appends.
const STALE_CHILD_NOTICE: &str = "Subagent child ids printed earlier in this session";
/// Marker the task tool prints before a resumable child's id.
const CHILD_ID_MARKER: &str = "[child id: ";
/// Most ids named in the notice.
const MAX_STALE_CHILD_IDS: usize = 8;

impl super::Session {
    /// Resumable subagent conversations are persisted with the session when
    /// they fit the resume store limits; a `[child id: ...]` line naming a
    /// child that was not restored would make `task` with `resume` fail with
    /// "unknown or expired". Append one system notice (once per resume, only
    /// for ids newer than the last notice and absent from the persisted store)
    /// so the model is not offered dead ids.
    /// Returns whether a notice was added (mini-agent-csk01).
    pub fn note_stale_child_ids(&mut self, resumed: bool) -> bool {
        if !resumed {
            return false;
        }
        let start = self
            .messages
            .iter()
            .rposition(|message| {
                message.role == super::MessageRole::System
                    && message.content.starts_with(STALE_CHILD_NOTICE)
            })
            .map_or(0, |index| index + 1);
        // Children persisted with the session are restored on first use, so
        // only ids missing from that store are reported as lost.
        #[cfg(feature = "subagents")]
        let restorable = crate::extras::subagents::resume::restorable_ids(self.id.as_str());
        let mut ids: Vec<String> = Vec::new();
        let mut total = 0usize;
        for message in &self.messages[start..] {
            if !matches!(
                message.role,
                super::MessageRole::ToolResult | super::MessageRole::SubagentToolCall
            ) {
                continue;
            }
            for (index, _) in message.content.match_indices(CHILD_ID_MARKER) {
                let rest = &message.content[index + CHILD_ID_MARKER.len()..];
                let id: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .take(64)
                    .collect();
                #[cfg(feature = "subagents")]
                if restorable.contains(&id) {
                    continue;
                }
                if id.is_empty() || ids.contains(&id) {
                    continue;
                }
                total += 1;
                if ids.len() < MAX_STALE_CHILD_IDS {
                    ids.push(id);
                }
            }
        }
        if total == 0 {
            return false;
        }
        let more = if total > ids.len() {
            format!(" (and {} more)", total - ids.len())
        } else {
            String::new()
        };
        let text = format!(
            "{STALE_CHILD_NOTICE} are no longer resumable: {}{more}. These subagent conversations were not kept with the session (they exceeded the resume store limits, were dropped, or the session artifacts were removed), so a task call with `resume` for any of them fails. Start a new task instead and pass what the child found as context.",
            ids.join(", ")
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

    #[test]
    fn restored_sessions_are_told_once_that_child_ids_are_dead() {
        let mut s = session();
        s.add_message(
            MessageRole::ToolResult,
            "report\n[child id: 1b2c-3d; continue this child's conversation with task {\"resume\": \"1b2c-3d\"}]",
        );
        // A live session keeps its store: nothing is said.
        assert!(!s.note_stale_child_ids(false));
        assert_eq!(system_notices(&s), 0);
        assert!(s.note_stale_child_ids(true));
        let text = s.messages.last().unwrap().content.to_string();
        assert!(
            text.contains("1b2c-3d") && text.contains("no longer resumable"),
            "{text}"
        );
        // The notice does not itself look like an offer, and a second resume
        // with no newer ids stays quiet.
        assert!(!text.contains("[child id:"));
        assert!(!s.note_stale_child_ids(true));
        assert_eq!(system_notices(&s), 1);
        // Ids from a later run are named by the next resume only.
        s.add_message(MessageRole::ToolResult, "[child id: ffff; continue]");
        assert!(s.note_stale_child_ids(true));
        let text = s.messages.last().unwrap().content.to_string();
        assert!(text.contains("ffff") && !text.contains("1b2c-3d"), "{text}");
        assert_eq!(system_notices(&s), 2);
        // Sessions without child ids are untouched.
        let mut plain = session();
        plain.add_message(MessageRole::ToolResult, "nothing here");
        assert!(!plain.note_stale_child_ids(true));
    }
}
