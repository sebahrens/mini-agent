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

/// Conversations kept per session (or per task tool without a session pool).
pub(crate) const MAX_RESUMABLE_CHILDREN: usize = 16;
/// Serialized size above which a conversation is not kept for resumption.
pub(crate) const MAX_RESUMABLE_BYTES: usize = 8 * 1024 * 1024;

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
    /// Keep `history` for resumption and return its id, or `None` when it is
    /// empty or larger than [`MAX_RESUMABLE_BYTES`].
    pub(crate) fn insert(
        &self,
        agent_type: Option<String>,
        history: Vec<Message>,
    ) -> Option<String> {
        if history.is_empty()
            || serde_json::to_vec(&history).map_or(true, |bytes| bytes.len() > MAX_RESUMABLE_BYTES)
        {
            return None;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        children.push_back(StoredChild {
            id: id.clone(),
            agent_type,
            history,
        });
        while children.len() > MAX_RESUMABLE_CHILDREN {
            children.pop_front();
        }
        Some(id)
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
}
