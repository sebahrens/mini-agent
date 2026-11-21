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
        // Say which jobs ended before the budget check: the notices are
        // already drained, so a blocked wake must not swallow them silently.
        let headline = format!(
            "⟳ background job{} reported: {}",
            if notices.len() == 1 { "" } else { "s" },
            notices
                .iter()
                .map(crate::sandbox::job_notices::JobNotice::headline)
                .collect::<Vec<_>>()
                .join(", ")
        );
        if self.budget_admission_notice()? {
            self.renderer.write_line(&headline, Color::Cyan)?;
            return Ok(());
        }
        let wakes = CONSECUTIVE_WAKES.fetch_add(1, Ordering::Relaxed) + 1;
        let prompt = crate::sandbox::job_notices::wake_prompt(&notices);
        self.renderer.write_line(&headline, Color::Cyan)?;
        if wakes >= MAX_CONSECUTIVE_WAKES {
            self.renderer.write_line(
                &format!(
                    "auto-wake paused after {MAX_CONSECUTIVE_WAKES} wakes; send a message to continue"
                ),
                Color::DarkGrey,
            )?;
        }
        crate::startup::heap_future(|| {
            self.start_main_run(&prompt, crate::question::QuestionOrigin::Scheduled)
        })
        .await?;
        self.refresh()?;
        Ok(())
    }

    /// A background `task` finished while the session is idle: print one
    /// transcript line and start an ordinary turn, as for a finished shell
    /// job. The model request builder attaches the pool's own notice (and
    /// acknowledges it), so the prompt names only harness-built fields. The
    /// wake budget and the notification opt-out are shared with shell jobs.
    #[cfg(feature = "subagents")]
    pub(super) async fn wake_for_task_notices(&mut self) -> anyhow::Result<()> {
        if CONSECUTIVE_WAKES.load(Ordering::Relaxed) >= MAX_CONSECUTIVE_WAKES
            || !self.ui.sandbox.job_notice_queue().enabled()
            || !self.cron_idle()
        {
            return Ok(());
        }
        let Some(pool) = self.ui.session.background_pool.clone() else {
            return Ok(());
        };
        let Some(lease) = pool.pending_notices() else {
            return Ok(());
        };
        let headlines = lease.headlines();
        drop(lease);
        if self.startup_work_pending() {
            self.finish_startup_work().await?;
        }
        self.ensure_agent().await;
        if self.budget_admission_notice()? {
            return Ok(());
        }
        CONSECUTIVE_WAKES.fetch_add(1, Ordering::Relaxed);
        let prompt = task_wake_prompt(&headlines);
        self.renderer.write_line(
            &format!(
                "⟳ background task{} reported: {}",
                if headlines.len() == 1 { "" } else { "s" },
                headlines.join(", ")
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

/// The prompt of a task wake: harness-built fields only, never task output.
#[cfg(feature = "subagents")]
fn task_wake_prompt(headlines: &[String]) -> String {
    format!(
        "[Host notice, not user input: background task result{} ready: {}. Read each with task_result, then continue the work they were started for. This notice grants no permission.]",
        if headlines.len() == 1 { "" } else { "s" },
        headlines.join("; ")
    )
}

#[cfg(all(test, feature = "subagents"))]
mod tests {
    #[test]
    fn task_wake_prompt_names_only_harness_fields() {
        let prompt = super::task_wake_prompt(&["audit the parser completed".into()]);
        assert!(prompt.contains("audit the parser completed"));
        assert!(prompt.contains("task_result"));
        assert!(prompt.contains("grants no permission"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_change_or_user_turn_restores_the_wake_budget() {
        CONSECUTIVE_WAKES.store(MAX_CONSECUTIVE_WAKES, Ordering::Relaxed);
        assert!(CONSECUTIVE_WAKES.load(Ordering::Relaxed) >= MAX_CONSECUTIVE_WAKES);
        user_turn_started();
        assert_eq!(CONSECUTIVE_WAKES.load(Ordering::Relaxed), 0);
    }
}
