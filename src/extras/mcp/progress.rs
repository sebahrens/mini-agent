//! Display-only routing of MCP `notifications/progress` to the live tool-call
//! preview. rmcp attaches a progress token to every request; a call registers
//! that token here together with the transcript id of the tool call, and the
//! connection handler forwards matching notifications as
//! [`AgentEvent::ToolProgress`]. Nothing here reaches the session or the model,
//! and server-supplied text is reduced to one bounded line of printable text.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use compact_str::CompactString;
use rmcp::model::{ProgressNotificationParam, ProgressToken};

use crate::event::AgentEvent;

/// Minimum spacing of previews for one call.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(100);
/// Longest message shown in a preview.
const MAX_MESSAGE_CHARS: usize = 200;
/// Most calls tracked at once on a connection; further calls simply show none.
const MAX_TRACKED_CALLS: usize = 256;

struct Sink {
    id: CompactString,
    events: tokio::sync::mpsc::Sender<AgentEvent>,
    last: Option<Instant>,
}

/// Progress tokens of the in-flight calls on one connection.
#[derive(Clone, Default)]
pub(crate) struct ProgressRegistry {
    sinks: Arc<Mutex<HashMap<ProgressToken, Sink>>>,
}

/// Unregisters its token on drop, whichever way the call ends.
pub(crate) struct ProgressGuard {
    registry: ProgressRegistry,
    token: ProgressToken,
}

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        self.registry
            .sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
    }
}

impl ProgressRegistry {
    pub(crate) fn register(
        &self,
        token: ProgressToken,
        id: CompactString,
        events: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Option<ProgressGuard> {
        let mut sinks = self.sinks.lock().unwrap_or_else(|e| e.into_inner());
        if sinks.len() >= MAX_TRACKED_CALLS {
            return None;
        }
        sinks.insert(
            token.clone(),
            Sink {
                id,
                events,
                last: None,
            },
        );
        Some(ProgressGuard {
            registry: self.clone(),
            token,
        })
    }

    /// Forward one notification to its call, if that call is being shown. A
    /// full event channel or an unknown token drops the preview silently.
    pub(crate) fn deliver(&self, params: &ProgressNotificationParam) {
        let mut sinks = self.sinks.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sink) = sinks.get_mut(&params.progress_token) else {
            return;
        };
        let now = Instant::now();
        if sink
            .last
            .is_some_and(|last| now.duration_since(last) < PREVIEW_INTERVAL)
        {
            return;
        }
        sink.last = Some(now);
        let text = render(params);
        let _ = sink.events.try_send(AgentEvent::ToolProgress {
            id: sink.id.clone(),
            text: CompactString::from(text),
        });
    }
}

fn number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else if value.is_finite() {
        format!("{value:.2}")
    } else {
        "?".into()
    }
}

/// One printable line: `progress p[/total][: message]`, message untrusted.
fn render(params: &ProgressNotificationParam) -> String {
    let mut line = format!("progress {}", number(params.progress));
    if let Some(total) = params.total {
        line.push('/');
        line.push_str(&number(total));
    }
    if let Some(message) = &params.message {
        let message: String = message
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(MAX_MESSAGE_CHARS)
            .collect();
        let message = message.trim();
        if !message.is_empty() {
            line.push_str(": ");
            line.push_str(message);
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::NumberOrString;

    fn token(n: i64) -> ProgressToken {
        ProgressToken(NumberOrString::Number(n))
    }

    fn param(n: i64, progress: f64, message: Option<&str>) -> ProgressNotificationParam {
        let mut p = ProgressNotificationParam::new(token(n), progress);
        p.total = Some(10.0);
        p.message = message.map(str::to_owned);
        p
    }

    #[tokio::test]
    async fn registered_calls_receive_sanitized_previews_and_others_none() {
        let registry = ProgressRegistry::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let guard = registry.register(token(1), "call-1".into(), tx).unwrap();
        registry.deliver(&param(2, 1.0, Some("other call")));
        assert!(rx.try_recv().is_err());
        registry.deliver(&param(1, 3.0, Some("\x1b[2Jdone\n\x07 step")));
        let Ok(AgentEvent::ToolProgress { id, text }) = rx.try_recv() else {
            panic!("expected a preview");
        };
        assert_eq!(id, "call-1");
        assert_eq!(text, "progress 3/10: [2Jdone   step");
        // Throttled: an immediate second notification is dropped.
        registry.deliver(&param(1, 4.0, None));
        assert!(rx.try_recv().is_err());
        drop(guard);
        tokio::time::sleep(PREVIEW_INTERVAL * 2).await;
        registry.deliver(&param(1, 5.0, None));
        assert!(rx.try_recv().is_err(), "unregistered after the call ends");
    }

    #[test]
    fn long_messages_and_non_finite_numbers_stay_bounded() {
        let long = "x".repeat(10_000);
        let line = render(&param(1, f64::NAN, Some(&long)));
        assert!(line.starts_with("progress ?/10: "));
        assert!(line.chars().count() <= 40 + MAX_MESSAGE_CHARS);
    }

    #[test]
    fn tracked_calls_are_capped() {
        let registry = ProgressRegistry::default();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let guards: Vec<_> = (0..MAX_TRACKED_CALLS as i64)
            .map(|n| registry.register(token(n), "c".into(), tx.clone()))
            .collect();
        assert!(guards.iter().all(Option::is_some));
        assert!(registry.register(token(9999), "c".into(), tx).is_none());
    }
}
