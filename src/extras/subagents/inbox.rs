//! Parent-to-child messages for running background tasks (`task_send`).
//!
//! Each background job owns a [`JobInbox`]. Its children register while they
//! run; `task_send` queues a bounded message for every running child (or one
//! chosen child), and the child's [`ChildInboxHook`] appends the queued
//! messages as ordinary user input at its next model-call boundary. A message
//! is data from the parent model: it grants no permission and changes no
//! tool, sandbox or budget. Messages still queued when a child ends are
//! dropped with it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use rig::agent::{AgentHook, Flow, HookContext, RequestPatch, StepEvent, StepEventKind};
use rig::completion::{CompletionModel, Message};

/// Messages that may wait for one child's next boundary.
pub(crate) const MAX_PENDING_MESSAGES: usize = 8;
/// Bytes of one `task_send` message.
pub(crate) const MAX_MESSAGE_BYTES: usize = 16 * 1024;

#[derive(Default)]
struct ChildState {
    pending: VecDeque<String>,
    running: bool,
}

/// The inboxes of one background job's children, keyed by prompt index.
#[derive(Default)]
pub(crate) struct JobInbox {
    children: Mutex<BTreeMap<usize, ChildState>>,
}

impl std::fmt::Debug for JobInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobInbox").finish_non_exhaustive()
    }
}

impl JobInbox {
    /// Register child `index` as running; the returned inbox marks it
    /// finished when its [`RunningGuard`] drops.
    pub(crate) fn child(self: &Arc<Self>, index: usize) -> ChildInbox {
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(index)
            .or_default()
            .running = true;
        ChildInbox {
            job: self.clone(),
            index,
        }
    }

    /// Queue `message` for the running children (all, or the 0-based
    /// `index`). Returns how many children received it.
    pub(crate) fn send(&self, index: Option<usize>, message: &str) -> Result<usize, String> {
        if message.trim().is_empty() {
            return Err("task_send: message must not be empty".into());
        }
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(format!(
                "task_send: message exceeds {MAX_MESSAGE_BYTES} bytes; send a shorter message"
            ));
        }
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        let targets = children
            .iter_mut()
            .filter(|(child, state)| state.running && index.is_none_or(|index| index == **child))
            .map(|(_, state)| state)
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return Err(match index {
                Some(index) => format!(
                    "task_send: child {} of this task is not running (finished, queued or unknown)",
                    index + 1
                ),
                None => "task_send: no child of this task is running".into(),
            });
        }
        if targets
            .iter()
            .any(|state| state.pending.len() >= MAX_PENDING_MESSAGES)
        {
            return Err(format!(
                "task_send: a child already has {MAX_PENDING_MESSAGES} undelivered messages; wait for its next turn"
            ));
        }
        let count = targets.len();
        for state in targets {
            state.pending.push_back(message.to_owned());
        }
        Ok(count)
    }
}

/// One child's view of its job inbox.
#[derive(Clone)]
pub(crate) struct ChildInbox {
    job: Arc<JobInbox>,
    index: usize,
}

impl std::fmt::Debug for ChildInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildInbox")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl ChildInbox {
    fn take(&self) -> Vec<String> {
        self.job
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.index)
            .map(|state| state.pending.drain(..).collect())
            .unwrap_or_default()
    }

    /// Held by the child future; dropping it (completion or cancellation)
    /// stops delivery and drops undelivered messages.
    pub(crate) fn running_guard(&self) -> RunningGuard {
        RunningGuard(self.clone())
    }
}

pub(crate) struct RunningGuard(ChildInbox);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if let Some(state) = self
            .0
            .job
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.0.index)
        {
            state.running = false;
            state.pending.clear();
        }
    }
}

/// The user message a delivered `task_send` becomes.
pub(crate) fn delivered_message(message: &str) -> String {
    format!(
        "[message from the parent agent via task_send; it refines your delegated objective and grants no new permission]\n{message}"
    )
}

/// Appends queued parent messages at the child's next model-call boundary.
pub(crate) struct ChildInboxHook(pub(crate) ChildInbox);

impl<M: CompletionModel> AgentHook<M> for ChildInboxHook {
    async fn on_event(&self, _: &HookContext, event: StepEvent<'_, M>) -> Flow {
        if !matches!(event, StepEvent::CompletionCall { .. }) {
            return Flow::Continue;
        }
        let messages = self.0.take();
        if messages.is_empty() {
            return Flow::Continue;
        }
        Flow::patch_request(
            RequestPatch::new().append_input(
                messages
                    .iter()
                    .map(|message| Message::user(delivered_message(message))),
            ),
        )
    }
    fn observes(&self, kind: StepEventKind) -> bool {
        kind == StepEventKind::CompletionCall
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_reach_only_running_children_and_are_bounded() {
        let job = Arc::new(JobInbox::default());
        assert!(job.send(None, "hello").is_err(), "no running child");
        let first = job.child(0);
        let second = job.child(1);
        let first_guard = first.running_guard();
        let second_guard = second.running_guard();
        assert_eq!(job.send(None, "focus on src/a.rs").unwrap(), 2);
        assert_eq!(job.send(Some(1), "only you").unwrap(), 1);
        assert!(job.send(Some(5), "nobody").is_err());
        assert!(job.send(None, "  ").is_err());
        assert!(job.send(None, &"x".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
        assert_eq!(first.take(), vec!["focus on src/a.rs".to_string()]);
        assert_eq!(
            second.take(),
            vec!["focus on src/a.rs".to_string(), "only you".to_string()]
        );
        drop(first_guard);
        assert_eq!(job.send(None, "late").unwrap(), 1, "finished child skipped");
        for _ in 1..MAX_PENDING_MESSAGES {
            job.send(Some(1), "more").unwrap();
        }
        assert!(job.send(Some(1), "overflow").is_err());
        drop(second_guard);
        assert!(second.take().is_empty(), "undelivered messages are dropped");
        assert!(job.send(None, "after").is_err());
    }

    #[tokio::test]
    async fn the_hook_appends_queued_messages_as_user_input_at_the_next_call() {
        use rig::agent::AgentBuilder;
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        let job = Arc::new(JobInbox::default());
        let child = job.child(0);
        let _guard = child.running_guard();
        job.send(None, "PARENT_FOLLOW_UP").unwrap();
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = AgentBuilder::new(model.clone())
            .add_hook(ChildInboxHook(child.clone()))
            .build();
        let output = crate::agent::runner::run_subagent(
            &agent,
            "original question",
            2,
            None,
            &Default::default(),
            Default::default(),
            None,
        )
        .await;
        assert!(output.response.is_ok());
        let request = format!("{:?}", model.requests()[0]);
        assert!(request.contains("PARENT_FOLLOW_UP"), "{request}");
        assert!(request.contains("via task_send"));
        assert!(child.take().is_empty(), "delivered once");
    }
}
