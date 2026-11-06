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
