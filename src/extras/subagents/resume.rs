//! Resumable child conversations.
//!
//! After a child returns a report, the host keeps its conversation (prompt,
//! tool calls, tool results and final text) under a fresh child id and
//! prints that id after the child's quoted output. A later
//! `task {"resume": "<id>", "prompts": ["follow-up"]}` call continues that
//! conversation with the follow-up as a new user turn, through the same
//! permission check, persona, tools, cost budget and report contract as any
//! other call. The store is bounded and in memory only: ids do not survive a
//! restart, and the oldest conversation is dropped first.

use std::collections::VecDeque;
use std::sync::Mutex;

use rig::completion::Message;

/// Default conversations kept per session (or per task tool without a session
/// pool). `[subagents] task_resume_max_children` overrides it.
pub(crate) const MAX_RESUMABLE_CHILDREN: usize = 16;
/// Default serialized size above which a conversation is not kept for
/// resumption. `[subagents] task_resume_max_bytes` overrides it.
pub(crate) const MAX_RESUMABLE_BYTES: usize = 8 * 1024 * 1024;
/// Hard ceilings for the configured limits.
const MAX_RESUMABLE_CHILDREN_CEILING: usize = 256;
const MAX_RESUMABLE_BYTES_CEILING: usize = 256 * 1024 * 1024;

/// Effective resume-store limits (configured, clamped, or the defaults).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResumeLimits {
    pub(crate) children: usize,
    pub(crate) bytes: usize,
}

impl Default for ResumeLimits {
    fn default() -> Self {
        Self {
            children: MAX_RESUMABLE_CHILDREN,
            bytes: MAX_RESUMABLE_BYTES,
        }
    }
}

impl ResumeLimits {
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        let defaults = Self::default();
        let Some(policy) = config.subagents.as_ref() else {
            return defaults;
        };
        Self {
            children: policy
                .task_resume_max_children
                .unwrap_or(defaults.children)
                .clamp(1, MAX_RESUMABLE_CHILDREN_CEILING),
            bytes: policy
                .task_resume_max_bytes
                .unwrap_or(defaults.bytes)
                .clamp(1024, MAX_RESUMABLE_BYTES_CEILING),
        }
    }
}

/// What [`ResumeStore::insert_with`] did with a finished conversation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Insert {
    /// Kept under `id`; `evicted` are the ids dropped to make room.
    Stored { id: String, evicted: Vec<String> },
    /// Nothing to resume (empty history).
    Empty,
    /// Not kept: the serialized conversation is larger than the byte limit.
    TooLarge { bytes: usize, limit: usize },
}

/// Host line telling the parent why a child cannot be resumed or what the
/// store dropped, so a missing or expired resume id is never a surprise.
pub(crate) fn insert_note(outcome: &Insert, limits: ResumeLimits) -> Option<String> {
    match outcome {
        Insert::Stored { evicted, .. } if !evicted.is_empty() => Some(format!(
            "[host note: the resume store keeps {} children ([subagents] task_resume_max_children); \
             dropped the oldest resumable child: {}]\n",
            limits.children,
            evicted.join(", ")
        )),
        Insert::TooLarge { bytes, limit } => Some(format!(
            "[host note: child not resumable: conversation exceeds {} ({} serialized); \
             raise [subagents] task_resume_max_bytes or start a new task]\n",
            human_mib(*limit),
            human_mib(*bytes)
        )),
        _ => None,
    }
}

fn human_mib(bytes: usize) -> String {
    if bytes >= 1024 * 1024 && bytes.is_multiple_of(1024 * 1024) {
        format!("{} MiB", bytes / (1024 * 1024))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KiB", bytes.div_ceil(1024))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct StoredChild {
    pub id: String,
    /// The `agent_type` the child ran as (`None`: the default explorer).
    pub agent_type: Option<String>,
    pub history: Vec<Message>,
}

#[derive(Debug, Default)]
pub(crate) struct ResumeStore {
    children: Mutex<VecDeque<StoredChild>>,
}

impl ResumeStore {
    /// Keep `history` for resumption under the default limits and return its
    /// id, or `None` when it is empty or too large.
    #[cfg(test)]
    pub(crate) fn insert(
        &self,
        agent_type: Option<String>,
        history: Vec<Message>,
    ) -> Option<String> {
        match self.insert_with(ResumeLimits::default(), agent_type, history) {
            Insert::Stored { id, .. } => Some(id),
            _ => None,
        }
    }

    pub(crate) fn insert_with(
        &self,
        limits: ResumeLimits,
        agent_type: Option<String>,
        history: Vec<Message>,
    ) -> Insert {
        if history.is_empty() {
            return Insert::Empty;
        }
        // An unserializable history is reported as over the limit rather than
        // silently kept or dropped.
        let bytes = serde_json::to_vec(&history).map_or(usize::MAX, |bytes| bytes.len());
        if bytes > limits.bytes {
            return Insert::TooLarge {
                bytes,
                limit: limits.bytes,
            };
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        children.push_back(StoredChild {
            id: id.clone(),
            agent_type,
            history,
        });
        let mut evicted = Vec::new();
        while children.len() > limits.children {
            if let Some(dropped) = children.pop_front() {
                evicted.push(dropped.id);
            }
        }
        Insert::Stored { id, evicted }
    }

    pub(crate) fn get(&self, id: &str) -> Option<StoredChild> {
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|child| child.id == id)
            .cloned()
    }
}

/// The host line printed after a resumable child's quoted output.
pub(crate) fn resume_line(id: &str) -> String {
    format!(
        "[child id: {id}; continue this child's conversation with task {{\"resume\": \"{id}\", \"prompts\": [\"<follow-up>\"]}}]\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_store_is_bounded_and_drops_the_oldest_conversation() {
        let store = ResumeStore::default();
        assert!(
            store.insert(None, Vec::new()).is_none(),
            "nothing to resume"
        );
        let ids = (0..=MAX_RESUMABLE_CHILDREN)
            .map(|index| {
                store
                    .insert(
                        Some("explore".into()),
                        vec![Message::user(format!("q{index}"))],
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(store.get(&ids[0]).is_none(), "oldest dropped");
        let last = store.get(ids.last().unwrap()).unwrap();
        assert_eq!(last.agent_type.as_deref(), Some("explore"));
        assert_eq!(last.history.len(), 1);
        let huge = vec![Message::user("x".repeat(MAX_RESUMABLE_BYTES))];
        assert!(store.insert(None, huge).is_none(), "oversized conversation");
    }

    #[test]
    fn oversize_and_eviction_are_reported_and_limits_are_configurable() {
        let store = ResumeStore::default();
        let small = ResumeLimits {
            children: 2,
            bytes: 2048,
        };
        let big = vec![Message::user("x".repeat(4096))];
        let outcome = store.insert_with(small, None, big);
        let Insert::TooLarge { bytes, limit } = outcome else {
            panic!("expected TooLarge, got {outcome:?}");
        };
        assert!(bytes > 4096 && limit == 2048);
        let note = insert_note(&outcome, small).unwrap();
        assert!(
            note.contains("child not resumable: conversation exceeds 2 KiB"),
            "{note}"
        );
        assert!(note.contains("task_resume_max_bytes"), "{note}");
        let mut ids = Vec::new();
        let mut last = None;
        for index in 0..3 {
            let outcome = store.insert_with(small, None, vec![Message::user(format!("q{index}"))]);
            let Insert::Stored { id, evicted } = &outcome else {
                panic!("expected Stored");
            };
            ids.push(id.clone());
            last = Some((insert_note(&outcome, small), evicted.clone()));
        }
        let (note, evicted) = last.unwrap();
        assert_eq!(evicted, vec![ids[0].clone()]);
        assert!(note.unwrap().contains(&ids[0]));
        assert!(store.get(&ids[0]).is_none() && store.get(&ids[2]).is_some());
        assert_eq!(
            insert_note(&store.insert_with(small, None, Vec::new()), small),
            None
        );
    }

    #[test]
    fn limits_come_from_config_with_ceilings() {
        let mut config = crate::config::Config::default();
        assert_eq!(ResumeLimits::from_config(&config), ResumeLimits::default());
        config.subagents = Some(crate::config::types::SubagentPolicy {
            task_resume_max_children: Some(0),
            task_resume_max_bytes: Some(usize::MAX),
            ..Default::default()
        });
        let limits = ResumeLimits::from_config(&config);
        assert_eq!(limits.children, 1);
        assert_eq!(limits.bytes, MAX_RESUMABLE_BYTES_CEILING);
        config.subagents = Some(crate::config::types::SubagentPolicy {
            task_resume_max_children: Some(40),
            task_resume_max_bytes: Some(1 << 20),
            ..Default::default()
        });
        assert_eq!(
            ResumeLimits::from_config(&config),
            ResumeLimits {
                children: 40,
                bytes: 1 << 20
            }
        );
    }
}
