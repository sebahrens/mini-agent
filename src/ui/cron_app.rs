//! The `/cron` fire path on the TUI [`App`].
//!
//! The event loop computes [`App::cron_wakeup`] before its `select!` and runs
//! one small arm when that instant passes; the arm awaits the boxed
//! [`App::fire_due_cron`], so the heavy turn start never sits in the event-loop
//! poll frame (the 1 MiB startup stack budget). A fire starts a turn only when
//! [`App::cron_idle`] holds; otherwise the expired deadline is parked as one
//! due fire that runs at the next idle iteration, after any queued user input.
use super::*;
use tokio::time::Instant;

/// Echo a scheduler fire as a host marker distinct from the user's `>` echo.
/// The marker is display-only; the session records the payload as an
/// ordinary User message.
fn echo_cron_fire(renderer: &mut Renderer, fire: &crate::ui::cron::CronFire) -> io::Result<()> {
    renderer.frame(|renderer| {
        let mut lines = fire.payload.lines();
        let first = lines.next().unwrap_or_default();
        renderer.write_line(
            &format!(
                "⟳ cron {} (every {}): {}",
                fire.id,
                crate::ui::cron::format_duration(fire.interval),
                sanitize_output(first)
            ),
            Color::Cyan,
        )?;
        for line in lines {
            renderer.write_line(&format!("  {}", sanitize_output(line)), Color::Cyan)?;
        }
        renderer.write_line("", Color::White)
    })
}

impl App<'_> {
    /// The TUI is truly idle: no run (a live goal counts as busy), no prompt,
    /// no pending Git or MCP operation, no queued or deferred input, nothing
    /// awaiting the user's answer. Every condition here is one under which a
    /// programmatic turn start would steal user state, be refused into the
    /// editor, or make `start_main_run` return an error that exits the TUI.
    pub(super) fn cron_idle(&self) -> bool {
        if self.run.is_running
            || self.permission_prompt.is_some()
            || self.question_prompt.is_some()
            || !self.run.pending_inputs.is_empty()
            || !self.deferred_user_events.is_empty()
            || self.ui.session.pending_question.is_some()
            || self.chain.plan.pending.is_some()
            || self.chain.pending.is_some()
            || self.renderer.chain_prompt.is_some()
            || self.run.native_owner.is_some()
            || self.run.native_blocked.is_some()
        {
            return false;
        }
        #[cfg(feature = "subagents")]
        if !self.background_requests.is_empty() {
            return false;
        }
        #[cfg(feature = "git-worktree")]
        if self.chain.git.work.is_some() || self.chain.git.pending.is_some() {
            return false;
        }
        #[cfg(feature = "mcp")]
        if self
            .mcp_admin
            .as_ref()
            .is_some_and(|state| state.work.is_some() || state.approval.is_some())
        {
            return false;
        }
        #[cfg(feature = "goal")]
        if self.ui.session.goal_store.is_live() {
            return false;
        }
        #[cfg(feature = "multimodal")]
        if !self.ui.session.pending_media.is_empty() {
            return false;
        }
        true
    }

    /// When the event loop's cron arm should wake: now when a parked fire can
    /// start, otherwise the earliest pending deadline. A parked fire while
    /// busy does not wake the loop; the next idle iteration recomputes this.
    pub(super) fn cron_wakeup(&self) -> Option<Instant> {
        if self.cron.is_empty() {
            return None;
        }
        if self.cron.has_due() && self.cron_idle() {
            return Some(Instant::now());
        }
        self.cron.next_deadline()
    }

    /// Park every expired deadline and, when idle, start the earliest due
    /// fire as an ordinary turn with the scheduler origin.
    pub(super) async fn fire_due_cron(&mut self) -> anyhow::Result<()> {
        self.cron.mark_expired_due(Instant::now());
        if !self.cron_idle() || !self.cron.has_due() {
            return Ok(());
        }
        if self.startup_work_pending() {
            self.finish_startup_work().await?;
            if !self.cron_idle() {
                return Ok(());
            }
        }
        let Some(id) = self.cron.next_due_id().map(str::to_owned) else {
            return Ok(());
        };
        self.ensure_agent().await;
        if self.budget_admission_notice()? {
            // Keep every entry (and its parked fire); one notice only.
            if !self.cron.is_suspended() {
                self.cron.suspend();
                self.renderer.write_line(
                    &format!(
                        "cron: USD budget reached; {} schedule(s) suspended and kept (/cron stop to remove)",
                        self.cron.len()
                    ),
                    C_ERROR,
                )?;
            }
            self.refresh()?;
            return Ok(());
        }
        let max_fires = self.ui.cfg.resolve_cron_max_fires();
        // The deadline moves before the turn starts, so a long turn cannot
        // double-fire.
        let Some(fire) = self.cron.begin_fire(&id, Instant::now(), max_fires) else {
            return Ok(());
        };
        // The fire count (or a retirement) is saved by the turn's own save.
        self.sync_cron_schedule();
        echo_cron_fire(&mut self.renderer, &fire)?;
        if fire.retired {
            self.renderer.write_line(
                &format!(
                    "cron {}: reached cron_max_fires ({max_fires}); schedule removed after this fire",
                    fire.id
                ),
                C_TOOL,
            )?;
        }
        crate::startup::heap_future(|| {
            self.start_main_run(&fire.payload, crate::question::QuestionOrigin::Scheduled)
        })
        .await?;
        self.cron_turn = self.run.is_running.then_some(fire.payload);
        self.refresh()?;
        Ok(())
    }

    /// Copy the live schedule into the session so the next ordinary save
    /// persists it. Quit and teardown stop the store without syncing, so the
    /// saved schedule survives them disarmed.
    pub(super) fn sync_cron_schedule(&mut self) {
        self.ui.session.cron_schedule = self.cron.export(Instant::now(), chrono::Utc::now());
    }

    /// The active session changed (`/resume`, `/new`, `/clear`, `/fork`,
    /// ...): schedules of the previous session end here (they were saved with
    /// it), and the new session's saved schedules are held disarmed behind one
    /// notice until `/cron resume`.
    pub(super) fn adopt_saved_cron(&mut self) -> io::Result<()> {
        let dropped = self.cron.len();
        self.cron = crate::ui::cron::CronStore::from_saved(&self.ui.session.cron_schedule);
        if dropped > 0 {
            self.renderer.write_line(
                &format!("cron: {dropped} schedule(s) of the previous session stopped"),
                C_TOOL,
            )?;
        }
        if let Some(notice) = crate::ui::cron::saved_schedule_notice(&self.ui.session.cron_schedule)
        {
            self.renderer
                .write_line(&sanitize_output(&notice), Color::Yellow)?;
        }
        Ok(())
    }

    /// Esc at an idle, empty editor: drop parked fires, keep the schedules.
    pub(super) fn clear_parked_cron_fires(&mut self) -> anyhow::Result<bool> {
        if !self.cron.has_due() {
            return Ok(false);
        }
        let cleared = self.cron.clear_due();
        self.renderer.write_line(
            &format!("cron: cleared {cleared} parked fire(s); schedules kept"),
            C_TOOL,
        )?;
        Ok(true)
    }
}
