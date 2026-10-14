//! Invocation-owned, bounded operator input consumed only at a model boundary.
use rig::agent::{AgentHook, Flow, HookContext, RequestPatch, StepEvent, StepEventKind};
use rig::completion::{CompletionModel, Message};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

const MAX_STEER_BYTES: usize = 64 * 1024;

#[derive(Clone, Default)]
pub(crate) struct Steering {
    pending: Arc<Mutex<Option<String>>>,
    boundary: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
}

impl Steering {
    pub(crate) fn pending_text(&self) -> Option<String> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub(crate) fn take_pending(&self) -> Option<String> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
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
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.is_some() {
            return Err("a steering message is already pending");
        }
        *pending = Some(text.to_owned());
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
        let pending = self
            .steering
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(prompt) = pending else {
            return Flow::Continue;
        };
        #[cfg(feature = "hooks")]
        let (prompt, notices) = {
            if let Err(error) = crate::spend::run::admit_current_effect().await {
                return Flow::Terminate {
                    reason: error.to_string(),
                };
            }
            // Keep the hook future owned through cancellation so synchronous
            // handlers observe the scope cancellation, kill, and reap before
            // the runner's cleanup channel closes.
            let gate = match crate::agent::runner::spawn_async_scoped(crate::startup::heap_future(
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
                    self.steering.take_pending();
                    let _ = self
                        .events
                        .send(crate::event::AgentEvent::SteerRejected { reason })
                        .await;
                    return Flow::Continue;
                }
            }
        };
        #[cfg(not(feature = "hooks"))]
        let notices: Vec<String> = Vec::new();
        let mut messages: Vec<Message> = notices.into_iter().map(Message::assistant).collect();
        messages.push(Message::user(prompt));
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
        assert!(steering.submit("second").is_err());
        assert_eq!(
            steering.pending.lock().unwrap().take().as_deref(),
            Some("first")
        );
        steering.submit("second").unwrap();
    }
}
