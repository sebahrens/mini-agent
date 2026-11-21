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

/// Start of the system notice [`Session::note_gone_background_work`] appends.
const GONE_WORK_NOTICE: &str = "Background shell jobs and tasks started earlier in this session";
const JOB_STARTED_MARKER: &str = "Background job started: ";
const JOB_LOG_MARKER: &str = "Complete output is logged to ";
const TASK_STARTED_MARKER: &str = "Background task started: ";

/// Job or task id characters.
fn id_at(text: &str) -> String {
    text.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect()
}

impl super::Session {
    /// A resumed session's transcript still says that a background shell job
    /// or task was started and is running, but both died with the previous
    /// process. Append one system notice naming those started after the last
    /// such notice and not recorded as finished, so the model does not wait on
    /// a dev server or test run that no longer exists. `live` says whether an
    /// id still belongs to this process (a session switched back to within one
    /// process keeps its jobs). Returns whether a notice was added
    /// (mini-agent-nvnkx.270).
    pub fn note_gone_background_work(
        &mut self,
        resumed: bool,
        live: impl Fn(&str) -> bool,
    ) -> bool {
        if !resumed {
            return false;
        }
        let start = self
            .messages
            .iter()
            .rposition(|message| {
                message.role == super::MessageRole::System
                    && message.content.starts_with(GONE_WORK_NOTICE)
            })
            .map_or(0, |index| index + 1);
        let segment = &self.messages[start..];
        // (id, log path) in start order.
        let mut jobs: Vec<(String, Option<String>)> = Vec::new();
        let mut tasks: Vec<String> = Vec::new();
        for message in segment {
            if !matches!(message.role, super::MessageRole::ToolResult) {
                continue;
            }
            let text = message.content.as_str();
            if let Some(at) = text.find(JOB_STARTED_MARKER) {
                let id = id_at(&text[at + JOB_STARTED_MARKER.len()..]);
                let log = text.find(JOB_LOG_MARKER).map(|at| {
                    text[at + JOB_LOG_MARKER.len()..]
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                });
                if !id.is_empty() && !jobs.iter().any(|(known, _)| *known == id) {
                    jobs.push((id, log.filter(|path| !path.is_empty())));
                }
            }
            if let Some(at) = text.find(TASK_STARTED_MARKER) {
                let id = id_at(&text[at + TASK_STARTED_MARKER.len()..]);
                if !id.is_empty() && !tasks.contains(&id) {
                    tasks.push(id);
                }
            }
        }
        if jobs.is_empty() && tasks.is_empty() {
            return false;
        }
        let mentions = |needle: &str| {
            segment
                .iter()
                .any(|message| message.content.contains(needle))
        };
        // A job whose status poll reported anything but running/stopping ended.
        jobs.retain(|(id, _)| {
            if live(id) {
                return false;
            }
            let marker = format!("Job: {id}\nStatus: ");
            !segment.iter().any(|message| {
                message.content.match_indices(&marker).any(|(at, _)| {
                    let status = id_at(&message.content[at + marker.len()..]);
                    !matches!(status.as_str(), "running" | "stopping")
                })
            })
        });
        #[cfg(feature = "subagents")]
        let rows = self.background_tasks();
        tasks.retain(|id| {
            if live(id) || mentions(&format!("[Background task {id} ")) {
                return false;
            }
            #[cfg(feature = "subagents")]
            if rows.iter().any(|row| {
                row.id == *id
                    && !row.status.is_active()
                    && row.status != crate::extras::subagents::background::Status::Interrupted
            }) {
                return false;
            }
            true
        });
        let total = jobs.len() + tasks.len();
        if total == 0 {
            return false;
        }
        let mut named: Vec<String> = jobs
            .iter()
            .map(|(id, log)| match log {
                Some(path) => format!("{id} (log: {path})"),
                None => id.clone(),
            })
            .collect();
        named.extend(tasks.iter().map(|id| format!("task {id}")));
        let more = if total > MAX_STALE_CHILD_IDS {
            format!(" (and {} more)", total - MAX_STALE_CHILD_IDS)
        } else {
            String::new()
        };
        named.truncate(MAX_STALE_CHILD_IDS);
        let text = format!(
            "{GONE_WORK_NOTICE} are no longer running: {}{more}. They ended with the previous process, so polling them fails and nothing will finish or notify. Treat their output as incomplete, and start them again if the work is still needed.",
            named.join(", ")
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

    #[test]
    fn restored_sessions_are_told_once_that_background_work_is_gone() {
        let mut s = session();
        let job = "job-6f1c0c5e-0000-4000-8000-000000000001";
        let done = "job-6f1c0c5e-0000-4000-8000-000000000002";
        let task = "6f1c0c5e-0000-4000-8000-000000000003";
        s.add_message(
            MessageRole::ToolResult,
            &format!("Background job started: {job}\nUse job_status.\nComplete output is logged to /tmp/shell-{job}.log"),
        );
        s.add_message(
            MessageRole::ToolResult,
            &format!("Background job started: {done}"),
        );
        s.add_message(
            MessageRole::ToolResult,
            &format!("Job: {done}\nStatus: completed\nCommand: true"),
        );
        s.add_message(
            MessageRole::ToolResult,
            &format!("Background task started: {task}. Its result is announced."),
        );
        assert!(!s.note_gone_background_work(false, |_| false));
        assert!(s.note_gone_background_work(true, |_| false));
        let text = s.messages.last().unwrap().content.to_string();
        assert!(text.contains(job) && text.contains("/tmp/shell-"), "{text}");
        assert!(text.contains(task), "{text}");
        assert!(!text.contains(done), "a finished job is not listed: {text}");
        // The second resume has nothing newer to report.
        assert!(!s.note_gone_background_work(true, |_| false));
        // A job still owned by this process is not reported.
        let mut live = session();
        live.add_message(
            MessageRole::ToolResult,
            &format!("Background job started: {job}"),
        );
        assert!(!live.note_gone_background_work(true, |id| id == job));
    }
}
