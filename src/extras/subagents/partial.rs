//! Live capture of a running child's narration.
//!
//! The scheduler drops in-flight child futures when a call stops at its
//! deadline or an aggregate output/cost limit. Without this capture their
//! streamed text would be lost and the parent would see only
//! `[cancelled: ...]`. The child runner appends each streamed text chunk and
//! counts tool calls through a task-local capture that the scheduler installs
//! around every child future; the scheduler reads it after cancellation and
//! returns the narration as an explicitly marked, quoted partial.

use std::future::Future;
use std::sync::{Arc, Mutex};

/// Bytes of narration retained per child; later text is counted only.
pub(crate) const MAX_PARTIAL_BYTES: usize = 32 * 1024;

#[derive(Clone, Default)]
pub(crate) struct PartialCapture(Arc<Mutex<PartialState>>);

#[derive(Default)]
struct PartialState {
    text: String,
    tool_calls: u32,
    truncated: bool,
}

/// What a cancelled child had produced when it was stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartialSnapshot {
    pub text: String,
    pub tool_calls: u32,
    pub truncated: bool,
}

tokio::task_local! {
    static CAPTURE: PartialCapture;
}

impl PartialCapture {
    /// Run `future` with this capture installed for the runner's hooks.
    pub(crate) fn scope<F: Future>(self, future: F) -> impl Future<Output = F::Output> {
        CAPTURE.scope(self, future)
    }

    /// The narration so far, or `None` when the child produced no text.
    pub(crate) fn snapshot(&self) -> Option<PartialSnapshot> {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (!state.text.trim().is_empty()).then(|| PartialSnapshot {
            text: state.text.clone(),
            tool_calls: state.tool_calls,
            truncated: state.truncated,
        })
    }

    fn push(&self, text: &str) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let room = MAX_PARTIAL_BYTES.saturating_sub(state.text.len());
        if text.len() <= room {
            state.text.push_str(text);
            return;
        }
        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        state.text.push_str(&text[..end]);
        state.truncated = true;
    }
}

/// The per-child time window (mini-agent-ss239): its length and the moment
/// the child's first model run started, which is when the window opens.
#[derive(Clone)]
pub(crate) struct ChildWindow {
    timeout: std::time::Duration,
    opened: Arc<Mutex<Option<tokio::time::Instant>>>,
}

tokio::task_local! {
    static WINDOW: Option<ChildWindow>;
}

/// Run `future` with a per-child window of `timeout`; the runner reads it
/// through [`child_deadline`].
pub(crate) fn with_child_window<F: Future>(
    timeout: std::time::Duration,
    future: F,
) -> impl Future<Output = F::Output> {
    WINDOW.scope(
        Some(ChildWindow {
            timeout,
            opened: Arc::default(),
        }),
        future,
    )
}

/// Run `future` outside any child window: the tool-free repair and write-up
/// turns that follow a deadline must not be cut by the deadline they report.
pub(crate) fn without_child_window<F: Future>(future: F) -> impl Future<Output = F::Output> {
    WINDOW.scope(None, future)
}

/// The deadline of the current child's window, opening it on first use.
pub(crate) fn child_deadline() -> Option<tokio::time::Instant> {
    WINDOW
        .try_with(|window| {
            let window = window.as_ref()?;
            let mut opened = window.opened.lock().unwrap_or_else(|e| e.into_inner());
            Some(*opened.get_or_insert_with(tokio::time::Instant::now) + window.timeout)
        })
        .ok()
        .flatten()
}

/// Record streamed child text (no-op outside a scheduled child).
pub(crate) fn record_text(text: &str) {
    let _ = CAPTURE.try_with(|capture| capture.push(text));
}

/// Count one child tool call (no-op outside a scheduled child).
pub(crate) fn record_tool_call() {
    let _ = CAPTURE.try_with(|capture| {
        let mut state = capture.0.lock().unwrap_or_else(|e| e.into_inner());
        state.tool_calls = state.tool_calls.saturating_add(1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_child_window_opens_once_and_can_be_suspended() {
        assert!(child_deadline().is_none());
        with_child_window(std::time::Duration::from_secs(10), async {
            let first = child_deadline().unwrap();
            tokio::time::advance(std::time::Duration::from_secs(4)).await;
            assert_eq!(child_deadline().unwrap(), first, "the window opens once");
            assert!(
                without_child_window(async { child_deadline() })
                    .await
                    .is_none()
            );
        })
        .await;
    }

    #[tokio::test]
    async fn partial_capture_records_text_only_inside_its_scope_and_is_bounded() {
        record_text("outside");
        let capture = PartialCapture::default();
        capture
            .clone()
            .scope(async {
                record_text("hello ");
                record_tool_call();
                record_text(&"x".repeat(MAX_PARTIAL_BYTES));
            })
            .await;
        let snapshot = capture.snapshot().unwrap();
        assert!(snapshot.text.starts_with("hello "));
        assert_eq!(snapshot.text.len(), MAX_PARTIAL_BYTES);
        assert_eq!(snapshot.tool_calls, 1);
        assert!(snapshot.truncated);
        assert!(PartialCapture::default().snapshot().is_none());
    }
}
