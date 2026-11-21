//! Invocation-owned, bounded operator input consumed only at a model boundary.
use rig::agent::{AgentHook, Flow, HookContext, RequestPatch, StepEvent, StepEventKind};
use rig::completion::{CompletionModel, Message};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

const MAX_STEER_BYTES: usize = 64 * 1024;

/// Bounded FIFO of operator messages. `delivering` counts the leading entries
/// a model boundary has snapshotted for injection; they stay queued (so a
/// failed handoff can restore them) until `consume_delivered` drops them.
#[derive(Default)]
struct Queue {
    messages: VecDeque<String>,
    bytes: usize,
    delivering: usize,
}

impl Queue {
    fn remove(&mut self, index: usize) -> Option<String> {
        let text = self.messages.remove(index)?;
        self.bytes -= text.len();
        Some(text)
    }
}

#[derive(Clone, Default)]
pub(crate) struct Steering {
    pending: Arc<Mutex<Queue>>,
    boundary: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
}

impl Steering {
    /// All queued messages joined by a blank line, oldest first.
    pub(crate) fn pending_text(&self) -> Option<String> {
        let queue = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        (!queue.messages.is_empty())
            .then(|| Vec::from_iter(queue.messages.iter().map(String::as_str)).join("\n\n"))
    }
    /// Drain every queued message (joined like `pending_text`).
    pub(crate) fn take_pending(&self) -> Option<String> {
        let mut queue = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let text = (!queue.messages.is_empty())
            .then(|| Vec::from_iter(queue.messages.iter().map(String::as_str)).join("\n\n"));
        *queue = Queue::default();
        text
    }
    /// Mark every queued message as being delivered at this boundary and
    /// return them oldest first, each to be appended as its own user message.
    pub(crate) fn snapshot_for_delivery(&self) -> Vec<String> {
        let mut queue = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        queue.delivering = queue.messages.len();
        queue.messages.iter().cloned().collect()
    }
    /// Drop the messages the last boundary snapshotted; later submissions stay.
    pub(crate) fn consume_delivered(&self) {
        let mut queue = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..std::mem::take(&mut queue.delivering) {
            queue.remove(0);
        }
    }
    pub(crate) fn submit_after_boundary(&self, text: &str) -> Result<(), &'static str> {
        self.enable();
        self.submit(text)?;
        self.boundary.store(true, Ordering::Release);
        Ok(())
    }
    pub(crate) fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }
    pub(crate) fn submit(&self, text: &str) -> Result<(), &'static str> {
        if !self.enabled.load(Ordering::Acquire) {
            return Err("this run does not accept steering");
        }
        if text.trim().is_empty() {
            return Err("usage: /steer <message>");
        }
        if text.len() > MAX_STEER_BYTES {
            return Err("steering message exceeds 64 KiB");
        }
        let mut queue = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if queue.bytes + text.len() > MAX_STEER_BYTES {
            return Err("queued steering messages would exceed 64 KiB");
        }
        queue.bytes += text.len();
        queue.messages.push_back(text.to_owned());
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct SteeringHook {
    pub(crate) steering: Steering,
    #[cfg_attr(not(feature = "hooks"), allow(dead_code))]
    pub(crate) events: tokio::sync::mpsc::Sender<crate::event::AgentEvent>,
}

impl Default for SteeringHook {
    fn default() -> Self {
        Self {
            steering: Steering::default(),
            events: tokio::sync::mpsc::channel(1).0,
        }
    }
}

impl<M: CompletionModel> AgentHook<M> for SteeringHook {
    async fn on_event(&self, _: &HookContext, event: StepEvent<'_, M>) -> Flow {
        if matches!(event, StepEvent::ToolResult { .. }) {
            self.steering.boundary.store(true, Ordering::Release);
            return Flow::Continue;
        }
        if !matches!(event, StepEvent::CompletionCall { .. })
            || !self.steering.boundary.swap(false, Ordering::AcqRel)
        {
            return Flow::Continue;
        }
        let snapshot = self.steering.snapshot_for_delivery();
        if snapshot.is_empty() {
            return Flow::Continue;
        }
        let mut messages: Vec<Message> = Vec::new();
        for prompt in snapshot {
            #[cfg(feature = "hooks")]
            let (prompt, notices) = {
                let prompt_text = prompt.clone();
                if let Err(error) = crate::spend::run::admit_current_effect().await {
                    return Flow::Terminate {
                        reason: error.to_string(),
                    };
                }
                // Keep the hook future owned through cancellation so synchronous
                // handlers observe the scope cancellation, kill, and reap before
                // the runner's cleanup channel closes.
                let gate =
                    match crate::agent::runner::spawn_async_scoped(crate::startup::heap_future(
                        move || crate::extras::hooks::dispatch_user_prompt_submit(prompt),
                    ))
                    .await
                    {
                        Ok(gate) => gate,
                        Err(error) => {
                            return Flow::Terminate {
                                reason: format!("steering hook failed: {error}"),
                            };
                        }
                    };
                match gate {
                    crate::extras::hooks::PromptGate::Proceed(prompt) => (prompt, Vec::new()),
                    crate::extras::hooks::PromptGate::WithContext { prompt, notices } => {
                        (prompt, notices)
                    }
                    crate::extras::hooks::PromptGate::Blocked(reason) => {
                        {
                            let mut queue = self
                                .steering
                                .pending
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let index = queue
                                .messages
                                .iter()
                                .position(|queued| *queued == prompt_text);
                            if let Some(index) = index {
                                queue.remove(index);
                                queue.delivering = queue.delivering.saturating_sub(1);
                            }
                        }
                        let _ = self
                            .events
                            .send(crate::event::AgentEvent::SteerRejected { reason })
                            .await;
                        continue;
                    }
                }
            };
            #[cfg(not(feature = "hooks"))]
            let notices: Vec<String> = Vec::new();
            messages.extend(
                notices
                    .into_iter()
                    .map(|notice| Message::user(crate::agent::input::host_context_text(&notice))),
            );
            messages.push(Message::user(prompt));
        }
        if messages.is_empty() {
            return Flow::Continue;
        }
        Flow::patch_request(RequestPatch::new().append_input(messages))
    }
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::CompletionCall | StepEventKind::ToolResult
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn steering_is_opt_in_bounded_and_never_overwrites_pending() {
        let steering = Steering::default();
        assert!(steering.submit("redirect").is_err());
        steering.enable();
        assert!(steering.submit("  ").is_err());
        assert!(steering.submit(&"x".repeat(MAX_STEER_BYTES + 1)).is_err());
        steering.submit("first").unwrap();
        steering.submit("second").unwrap();
        assert_eq!(steering.pending_text().as_deref(), Some("first\n\nsecond"));
        assert!(steering.submit(&"y".repeat(MAX_STEER_BYTES)).is_err());
        assert_eq!(steering.take_pending().as_deref(), Some("first\n\nsecond"));
        steering.submit("third").unwrap();
    }

    #[test]
    fn consume_delivered_keeps_messages_queued_after_the_snapshot() {
        let steering = Steering::default();
        steering.enable();
        steering.submit("a").unwrap();
        steering.submit("b").unwrap();
        steering.pending.lock().unwrap().delivering = 2;
        steering.submit("c").unwrap();
        steering.consume_delivered();
        assert_eq!(steering.pending_text().as_deref(), Some("c"));
        assert_eq!(steering.pending.lock().unwrap().bytes, 1);
    }

    #[test]
    fn boundary_snapshot_carries_every_queued_message_in_order() {
        let steering = Steering::default();
        steering.enable();
        steering.submit("one").unwrap();
        steering.submit("two").unwrap();
        assert_eq!(steering.snapshot_for_delivery(), vec!["one", "two"]);
        steering.consume_delivered();
        assert!(steering.pending_text().is_none());
    }
}
