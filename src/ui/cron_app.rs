//! The `/cron` fire path on the TUI [`App`].
//!
//! The event loop computes [`App::cron_wakeup`] before its `select!` and runs
//! one small arm when that instant passes; the arm awaits the boxed
//! [`App::fire_due_cron`], so the heavy turn start never sits in the event-loop
//! poll frame (the 1 MiB startup stack budget). A fire starts a turn only when
//! [`App::cron_idle`] holds; otherwise the expired deadline is parked as one
//! due fire that runs at the next idle iteration, after any queued user input.
//!
//! A self-paced (dynamic) entry's turn is offered the turn-scoped
//! `schedule_wakeup` tool; [`App::settle_dynamic_cron_turn`] removes it when
//! that turn ends and applies the requested delay, or retires the entry when
//! the turn made no call. Consecutive noop fires of one entry collapse into a
//! single updated feed line. An allowlisted slash payload runs through the
//! slash dispatcher at idle and never starts a model request.
use super::*;
use crate::agent::tools::schedule_wakeup::{ScheduleWakeup, Wakeup, WakeupSlot};
use tokio::time::Instant;

/// Feed position before a fire's echo, so a collapsed noop fire can retract
/// exactly what it wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FeedMark {
    blocks: usize,
    generation: u64,
}

impl FeedMark {
    fn of(renderer: &Renderer) -> Self {
        let feed = renderer.feed();
        Self {
            blocks: feed.block_count(),
            generation: feed.retention_generation(),
        }
    }
}

/// A self-paced fire whose turn is running.
pub(super) struct DynamicCronTurn {
    id: String,
    slot: WakeupSlot,
    mark: FeedMark,
}

/// The last noop summary line written for an entry.
pub(super) struct NoopLine {
    id: String,
    /// Index of the summary block, valid while `generation` is unchanged.
    block: usize,
    generation: u64,
    count: u32,
}

/// Echo a scheduler fire as a host marker distinct from the user's `>` echo.
/// The marker is display-only; the session records the payload as an
/// ordinary User message.
fn echo_cron_fire(renderer: &mut Renderer, fire: &crate::ui::cron::CronFire) -> io::Result<()> {
    renderer.frame(|renderer| {
        let mut lines = fire.payload.lines();
        let first = lines.next().unwrap_or_default();
        let pace = if fire.dynamic {
            "self-paced".to_owned()
        } else {
            format!("every {}", crate::ui::cron::format_duration(fire.interval))
        };
        renderer.write_line(
            &format!("⟳ cron {} ({pace}): {}", fire.id, sanitize_output(first)),
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

    /// The `cron` statusline item: `waiting` while the running scheduled
    /// turn holds a permission or question prompt.
    pub(super) fn cron_status_label(&self) -> Option<String> {
        let waiting = self.cron_turn.is_some()
            && self.run.is_running
            && (self.permission_prompt.is_some() || self.question_prompt.is_some());
        crate::ui::statusline::cron_label(self.cron.summary(Instant::now()), waiting)
    }

    /// Park every expired deadline and, when idle, start the earliest due
    /// fire as an ordinary turn with the scheduler origin.
    pub(super) async fn fire_due_cron(&mut self) -> anyhow::Result<()> {
        let parked = self.cron.mark_expired_due(Instant::now());
        if !self.cron_idle() {
            if parked > 0 && !self.cron.is_suspended() {
                // Transient only: the feed records the fire when it runs.
                self.renderer
                    .show_notice("cron: fire parked until idle", Color::Cyan);
                self.refresh()?;
            }
            return Ok(());
        }
        if !self.cron.has_due() {
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
        if self.cron.get(&id).is_some_and(|entry| {
            !entry.dynamic && crate::ui::slash::cron::is_allowlisted_slash_payload(&entry.payload)
        }) {
            return self.fire_cron_slash(&id).await;
        }
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
        let mark = FeedMark::of(&self.renderer);
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
        if fire.dynamic {
            let slot = WakeupSlot::default();
            self.run.cron_wakeup = Some(slot.clone());
            self.cron_dynamic = Some(DynamicCronTurn {
                id: fire.id.clone(),
                slot,
                mark,
            });
        }
        crate::startup::heap_future(|| {
            self.start_main_run(&fire.payload, crate::question::QuestionOrigin::Scheduled)
        })
        .await?;
        self.cron_turn = self.run.is_running.then_some(fire.payload);
        if !self.run.is_running {
            // The turn never started, so no wakeup can have been chosen.
            self.settle_dynamic_cron_turn().await?;
        }
        self.refresh()?;
        Ok(())
    }

    /// Fire an allowlisted read-only slash payload through the slash
    /// dispatcher. It starts no model request, so neither the agent nor the
    /// budget gate is involved, and none of the generic slash settlement
    /// (history save, goal auto-start) runs.
    async fn fire_cron_slash(&mut self, id: &str) -> anyhow::Result<()> {
        let max_fires = self.ui.cfg.resolve_cron_max_fires();
        let Some(fire) = self.cron.begin_fire(id, Instant::now(), max_fires) else {
            return Ok(());
        };
        self.cron_noop = None;
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
        let result = crate::startup::heap_future(|| {
            crate::ui::slash::handle_slash(
                &fire.payload,
                None,
                &mut self.renderer,
                &mut self.input,
                &mut self.run,
                &mut self.ui,
                &mut self.slash,
                &mut self.chain,
                &mut self.terminal_guard,
                &mut self.cron,
            )
        })
        .await;
        if let Err(error) = result {
            self.renderer
                .write_line(&sanitize_output(&error.to_string()), C_ERROR)?;
        }
        self.refresh()?;
        Ok(())
    }

    /// End a self-paced fire's turn: remove `schedule_wakeup` from the agent,
    /// then apply the turn's last call (or retire the entry without one). A
    /// no-op when no self-paced turn is running.
    pub(super) async fn settle_dynamic_cron_turn(&mut self) -> anyhow::Result<()> {
        let Some(turn) = self.cron_dynamic.take() else {
            return Ok(());
        };
        self.run.cron_wakeup = None;
        if let Some(agent) = self.run.agent.as_ref()
            && let Err(error) = agent
                .remove_turn_tool(<ScheduleWakeup as rig::tool::Tool>::NAME)
                .await
        {
            tracing::warn!(%error, "cron: failed to remove schedule_wakeup");
        }
        let wakeup = turn.slot.take();
        if self.cron.get(&turn.id).is_none() {
            // Stopped, or retired at cron_max_fires, while the turn ran.
            self.cron_noop = None;
            return Ok(());
        }
        let Some(wakeup) = wakeup else {
            self.cron.stop(&turn.id);
            self.sync_cron_schedule();
            self.cron_noop = None;
            self.renderer.write_line(
                &format!(
                    "cron {}: the turn ended without schedule_wakeup; self-paced schedule stopped",
                    turn.id
                ),
                C_TOOL,
            )?;
            return Ok(());
        };
        self.cron
            .schedule_wakeup(&turn.id, wakeup.delay, Instant::now());
        self.sync_cron_schedule();
        if wakeup.noop {
            self.write_cron_noop(&turn, &wakeup)?;
        } else {
            self.cron_noop = None;
            self.renderer.write_line(
                &format!(
                    "⟳ cron {}: next wakeup in {} · {}",
                    turn.id,
                    crate::ui::cron::format_duration(wakeup.delay),
                    sanitize_output(&wakeup.reason)
                ),
                Color::Cyan,
            )?;
        }
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

    /// Show a noop fire as one summary line. When the previous fire of the
    /// same entry was also a noop and nothing else was written since its
    /// line, this fire's echo and output are retracted and that line is
    /// rewritten with a count instead. The session keeps every message.
    fn write_cron_noop(&mut self, turn: &DynamicCronTurn, wakeup: &Wakeup) -> io::Result<()> {
        let generation = self.renderer.feed().retention_generation();
        let count = match self.cron_noop.take() {
            Some(previous)
                if previous.id == turn.id
                    && previous.generation == generation
                    && turn.mark.generation == generation
                    && turn.mark.blocks == previous.block + 1 =>
            {
                self.renderer.retract_feed_to(previous.block);
                previous.count.saturating_add(1)
            }
            _ => 1,
        };
        let times = if count > 1 {
            format!(" ×{count}")
        } else {
            String::new()
        };
        self.renderer.write_line(
            &format!(
                "⟳ cron {}: no change{times} · next in {} · {}",
                turn.id,
                crate::ui::cron::format_duration(wakeup.delay),
                sanitize_output(&wakeup.reason)
            ),
            Color::Cyan,
        )?;
        let feed = self.renderer.feed();
        self.cron_noop = Some(NoopLine {
            id: turn.id.clone(),
            block: feed.block_count().saturating_sub(1),
            generation: feed.retention_generation(),
            count,
        });
        Ok(())
    }

    /// Esc at an idle, empty editor: drop parked fires (interval schedules
    /// are kept) and pending self-paced wakeups (which ends those entries,
    /// as they have no interval to fall back to).
    pub(super) fn clear_parked_cron_fires(&mut self) -> anyhow::Result<bool> {
        if !self.cron.has_due() && !self.cron.has_dynamic_wakeup() {
            return Ok(false);
        }
        let stopped = self.cron.clear_dynamic_wakeups();
        let cleared = self.cron.clear_due();
        if !stopped.is_empty() {
            self.sync_cron_schedule();
        }
        if !stopped.is_empty() {
            self.cron_noop = None;
            self.renderer.write_line(
                &format!(
                    "cron: cleared the pending wakeup of {}; self-paced schedule(s) stopped",
                    stopped.join(", ")
                ),
                C_TOOL,
            )?;
        }
        if cleared > 0 {
            self.renderer.write_line(
                &format!("cron: cleared {cleared} parked fire(s); schedules kept"),
                C_TOOL,
            )?;
        }
        Ok(true)
    }
}
