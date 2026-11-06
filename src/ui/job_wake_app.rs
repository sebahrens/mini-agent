//! Wake an idle TUI session when a background shell job ends on its own
//! (mini-agent-cw4f3).
//!
//! The sandbox queues one bounded notice per job that ends without being
//! stopped. While the session is truly idle ([`App::cron_idle`]) the event
//! loop starts an ordinary turn whose prompt names only harness-built fields
//! (job id, status, exit code, log path), never program output. A turn that
//! is running or a prompt that is open leaves the notices queued: the next
//! tool result or model request carries them instead. After
//! [`MAX_CONSECUTIVE_WAKES`] wakes with no user turn in between, the session
//! stops waking itself and the notices wait for the user.
use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

/// Wakes in a row, without a user-typed turn between, before waking stops.
const MAX_CONSECUTIVE_WAKES: u32 = 8;

static CONSECUTIVE_WAKES: AtomicU32 = AtomicU32::new(0);

/// A user-typed turn resets the wake budget.
pub(super) fn user_turn_started() {
    CONSECUTIVE_WAKES.store(0, Ordering::Relaxed);
}

impl App<'_> {
    /// Whether the event loop should wait for a job notice to wake the
    /// session: notifications on, budget of wakes left, and nothing busy.
    pub(super) fn job_wake_armed(&self) -> bool {
        CONSECUTIVE_WAKES.load(Ordering::Relaxed) < MAX_CONSECUTIVE_WAKES
            && self.ui.sandbox.job_notice_queue().enabled()
            && self.cron_idle()
    }

    /// Take the pending notices and start a turn that tells the model.
    pub(super) async fn wake_for_job_notices(&mut self) -> anyhow::Result<()> {
        if !self.job_wake_armed() {
            return Ok(());
        }
        let notices = self.ui.sandbox.job_notice_queue().take();
        if notices.is_empty() {
            return Ok(());
        }
        if self.startup_work_pending() {
            self.finish_startup_work().await?;
        }
        self.ensure_agent().await;
        if self.budget_admission_notice()? {
            return Ok(());
        }
        CONSECUTIVE_WAKES.fetch_add(1, Ordering::Relaxed);
        let prompt = crate::sandbox::job_notices::wake_prompt(&notices);
        self.renderer.write_line(
            &format!(
                "⟳ background job{} reported: {}",
                if notices.len() == 1 { "" } else { "s" },
                notices
                    .iter()
                    .map(crate::sandbox::job_notices::JobNotice::headline)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Color::Cyan,
        )?;
        crate::startup::heap_future(|| {
            self.start_main_run(&prompt, crate::question::QuestionOrigin::Scheduled)
        })
        .await?;
        self.refresh()?;
        Ok(())
    }
}
