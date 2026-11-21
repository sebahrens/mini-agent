//! `/share confirm` as a cancellable background upload.
//!
//! The upload can take up to its request timeout. Running it inline would
//! hold the event loop (no keys, no redraw) for that long, so it runs as a
//! task that reports through [`UserEvent::ShareDone`]; Esc aborts it.
use super::*;
use std::sync::Mutex;

/// The running upload, if any. One at a time: a second `/share confirm`
/// while one is active is refused.
static ACTIVE: Mutex<Option<tokio::task::AbortHandle>> = Mutex::new(None);

type Slot = Mutex<Option<tokio::task::AbortHandle>>;

fn lock(slot: &'static Slot) -> std::sync::MutexGuard<'static, Option<tokio::task::AbortHandle>> {
    slot.lock().unwrap_or_else(|error| error.into_inner())
}

fn active() -> std::sync::MutexGuard<'static, Option<tokio::task::AbortHandle>> {
    lock(&ACTIVE)
}

/// Spawn `work` and record its abort handle in `slot`, which the task clears
/// when it finishes. The slot stays locked from before the spawn until the
/// handle is stored, so a task that finishes without suspending cannot clear
/// the slot first and leave a finished task's handle behind for good
/// (mini-agent-nvnkx.261).
fn spawn_tracked<F>(slot: &'static Slot, work: F) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let mut guard = lock(slot);
    let task = tokio::spawn(async move {
        work.await;
        *lock(slot) = None;
    });
    *guard = Some(task.abort_handle());
    drop(guard);
    task
}

/// Whether the submitted text is the confirming form of `/share`.
pub(super) fn is_confirm(text: &str) -> bool {
    matches!(
        text.split_whitespace().collect::<Vec<_>>().as_slice(),
        ["/share", "confirm"]
    )
}

/// Abort the running upload. Returns whether one was running.
pub(super) fn cancel_active() -> bool {
    match active().take() {
        Some(handle) => {
            handle.abort();
            true
        }
        None => false,
    }
}

impl App<'_> {
    /// Render the export and upload it in the background.
    pub(super) fn start_share(&mut self) -> anyhow::Result<()> {
        if active().is_some() {
            self.renderer.write_line(
                "a share upload is already running (Esc cancels it)",
                C_ERROR,
            )?;
            return Ok(());
        }
        let session = &*self.ui.session;
        let filename = crate::ui::slash::default_export_filename(&session.id);
        let html = crate::extras::export::session_to_html(session);
        let description = if session.name.is_empty() {
            "mini-agent session".to_string()
        } else {
            format!("mini-agent session: {}", session.name)
        };
        self.renderer.write_line(
            &format!(
                "uploading {} to gist.github.com… (Esc cancels)",
                crate::byte_text::format_bytes(html.len() as u64)
            ),
            C_AGENT,
        )?;
        let tx = self.user_tx.clone();
        spawn_tracked(&ACTIVE, async move {
            let result = crate::extras::export::share_gist(&filename, &html, &description)
                .await
                .map_err(|error| error.to_string());
            // Cleared before the event is sent, so a later `/share confirm`
            // never sees the finished upload.
            *active() = None;
            let _ = tx.send(UserEvent::ShareDone(result)).await;
        });
        Ok(())
    }

    /// Report a finished upload.
    pub(super) fn finish_share(&mut self, result: Result<String, String>) -> anyhow::Result<()> {
        match result {
            Ok(url) => self
                .renderer
                .write_line(&format!("shared as secret gist: {url}"), C_AGENT)?,
            Err(error) => self
                .renderer
                .write_line(&format!("share failed: {error}"), C_ERROR)?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_confirming_form_starts_an_upload() {
        assert!(is_confirm("/share confirm"));
        assert!(is_confirm("  /share   confirm "));
        assert!(!is_confirm("/share"));
        assert!(!is_confirm("/share confirm now"));
        assert!(!is_confirm("/share other"));
    }

    #[tokio::test]
    async fn cancelling_aborts_the_running_upload_once() {
        assert!(!cancel_active());
        let task = tokio::spawn(std::future::pending::<()>());
        *active() = Some(task.abort_handle());
        assert!(cancel_active());
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!cancel_active());
    }

    static RACE_SLOT: Slot = Mutex::new(None);

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_task_that_finishes_at_once_never_leaves_its_handle_behind() {
        for _ in 0..3000 {
            let task = spawn_tracked(&RACE_SLOT, async {});
            task.await.unwrap();
            assert!(lock(&RACE_SLOT).is_none(), "finished task kept its handle");
        }
    }
}
