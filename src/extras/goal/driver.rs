//! Running a goal: rounds, and what carries between them.
//!
//! A goal advances in rounds. One round is one agent run followed by one gate
//! evaluation. The driver lives here rather than in the runner because the
//! runner has no session to persist to, no client to ask a judge with, and
//! per-response budgets that fail hard rather than winding down — none of which
//! a goal can work with. At this layer every round starts with a fresh turn and
//! token budget, so the existing per-response bounds keep protecting a single
//! runaway round while the goal's own bounds cover the whole objective.
//!
//! [`RoundCollector`] turns a run's event stream into the facts the gate needs,
//! and is deliberately independent of the TUI, headless, and ACP surfaces so
//! all three agree on what a round was.
//!
//! Owning specification: `docs/superpowers/specs/2026-09-11-goal-feature-design.md`
//! §4.4 (driver evaluation) and §4.8 (continuation modes).

use std::time::Duration;

use super::gate::{GateDecision, RoundEnd, RoundSummary};
use super::{ContinuationMode, Goal, GoalStatus, ReportStatus};
use crate::event::AgentEvent;

/// Accumulates one round's facts from the agent's event stream.
///
/// The mutation count is per round. The runner keeps its own
/// `workspace_may_have_changed` flag, but that one is sticky for a whole run
/// and never resets, so reusing it would make stall detection dead from the
/// first edit onward.
#[derive(Debug, Default, Clone)]
pub struct RoundCollector {
    tool_calls: u32,
    mutating_tool_calls: u32,
    verify_ran: bool,
    verify_passed: Option<bool>,
    input_tokens: u64,
    output_tokens: u64,
    failure: Option<String>,
    /// Wall time the agent was actually working, excluding any stretch spent
    /// waiting on a permission prompt. A user at lunch must not consume a
    /// goal's time budget.
    active: Duration,
    running_since: Option<std::time::Instant>,
    blocked_since: Option<std::time::Instant>,
}

impl RoundCollector {
    pub fn new() -> Self {
        Self {
            running_since: Some(std::time::Instant::now()),
            ..Self::default()
        }
    }

    /// Fold one event into the round.
    pub fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolCall { name, .. } => {
                self.tool_calls += 1;
                if crate::agent::runner::tool_may_mutate_workspace(name.as_str()) {
                    self.mutating_tool_calls += 1;
                }
            }
            AgentEvent::Verification { passed, .. } => {
                self.verify_ran = true;
                self.verify_passed = Some(*passed);
            }
            AgentEvent::UsageDelta { usage, .. } => {
                self.input_tokens += usage.input_tokens;
                self.output_tokens += usage.output_tokens;
            }
            AgentEvent::Error { message, .. } => {
                self.failure = Some(message.to_string());
            }
            _ => {}
        }
    }

    /// The agent is blocked on a permission prompt; stop the clock.
    pub fn pause_clock(&mut self) {
        if self.blocked_since.is_none() {
            self.accrue();
            self.blocked_since = Some(std::time::Instant::now());
        }
    }

    /// The prompt was answered; restart the clock.
    pub fn resume_clock(&mut self) {
        if self.blocked_since.take().is_some() {
            self.running_since = Some(std::time::Instant::now());
        }
    }

    fn accrue(&mut self) {
        if let Some(started) = self.running_since.take() {
            self.active += started.elapsed();
        }
    }

    /// Close the round and produce what the gate reads.
    pub fn finish(mut self, goal: &Goal, end: RoundEnd, open_todos: usize) -> RoundSummary {
        self.accrue();
        let round = goal.progress.rounds + 1;
        // Only a report filed during this round speaks for it.
        let report = goal
            .reports_in_round(round)
            .last()
            .cloned()
            .or_else(|| goal.reports_in_round(round).next().cloned());
        RoundSummary {
            end: match end {
                // A failure the stream reported wins over a generic "done":
                // the runner emits `Error` and still closes the channel.
                RoundEnd::Done => match self.failure.take() {
                    Some(message) => RoundEnd::Failed(message),
                    None => RoundEnd::Done,
                },
                other => other,
            },
            tool_calls: self.tool_calls,
            mutating_tool_calls: self.mutating_tool_calls,
            report,
            verify_ran: self.verify_ran,
            verify_passed: self.verify_passed,
            verify_configured: false,
            open_todos,
            tokens_used: self.input_tokens + self.output_tokens,
            active_secs: self.active.as_secs(),
        }
    }
}

/// What history the next round starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryMode {
    /// Keep the conversation so far.
    Retained,
    /// Start clean; the objective and a carried summary are the whole prompt.
    Empty,
}

/// Everything needed to launch the next round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relaunch {
    pub prompt: String,
    pub history: HistoryMode,
    /// Provider-call cap for this round. Only the wrap-up round sets one, so
    /// an agent told to land its work cannot start a new project instead.
    pub max_agent_turns: Option<u32>,
}

/// Build the next round from a `Continue` decision.
///
/// Returns `None` for a `Stop`, which is the driver's signal to hand control
/// back to the user.
pub fn next_round(goal: &Goal, decision: &GateDecision) -> Option<Relaunch> {
    let GateDecision::Continue {
        instruction,
        wrap_up,
        ..
    } = decision
    else {
        return None;
    };

    let round = goal.progress.rounds + 1;
    let mut prompt = String::with_capacity(instruction.len() + 512);

    match goal.continuation {
        ContinuationMode::Continue => {
            prompt.push_str(&round_header(goal, round));
            prompt.push_str("\n\n");
            prompt.push_str(instruction);
            Some(Relaunch {
                prompt,
                history: HistoryMode::Retained,
                max_agent_turns: wrap_up.then_some(goal.bounds.wrap_up_max_agent_turns),
            })
        }
        ContinuationMode::Restart { summary_chars } => {
            // A fresh run has no memory of the conversation, so the objective
            // has to travel with the prompt.
            prompt.push_str("## Goal\n");
            prompt.push_str(&goal.objective);
            if !goal.criteria.is_empty() {
                prompt.push_str("\n\nDone when:");
                for criterion in &goal.criteria {
                    prompt.push_str("\n- ");
                    prompt.push_str(criterion);
                }
            }
            prompt.push_str("\n\n");
            prompt.push_str(&round_header(goal, round));
            if let Some(summary) = carried_summary(goal, summary_chars) {
                prompt.push_str("\n\nWhat happened so far:\n");
                prompt.push_str(&summary);
            }
            prompt.push_str("\n\n");
            prompt.push_str(instruction);
            Some(Relaunch {
                prompt,
                history: HistoryMode::Empty,
                max_agent_turns: wrap_up.then_some(goal.bounds.wrap_up_max_agent_turns),
            })
        }
    }
}

fn round_header(goal: &Goal, round: u32) -> String {
    format!("--- Goal round {round} of {} ---", goal.bounds.max_rounds)
}

/// Summary carried into a restarted round.
///
/// Built by the harness from the goal's own record, never from free model
/// prose. A model asked to summarize its way into the next round is a model
/// given the chance to quietly restate the objective as something smaller.
fn carried_summary(goal: &Goal, budget: usize) -> Option<String> {
    if budget == 0 {
        return None;
    }
    let mut lines = Vec::new();
    for report in goal.reports.iter().rev().take(4) {
        let text = match report.status {
            ReportStatus::Progress | ReportStatus::Met => report.evidence.as_deref(),
            ReportStatus::Blocked => report.blocker.as_deref(),
            ReportStatus::Impossible => report.reason.as_deref(),
            ReportStatus::NeedsUser => report.question.as_deref(),
        };
        if let Some(text) = text {
            lines.push(format!("- round {}: {text}", report.round));
        }
    }
    lines.reverse();
    if let Some(verdict) = &goal.last_verdict
        && !verdict.reason.is_empty()
    {
        lines.push(format!("- last check: {}", verdict.reason));
    }
    if lines.is_empty() {
        return None;
    }
    let mut out = lines.join("\n");
    if out.chars().count() > budget {
        out = out.chars().take(budget).collect::<String>();
        out.push('…');
    }
    Some(out)
}

/// One line per gate decision, shown in the conversation feed and on stderr.
///
/// Without it a user watching an agent keep going has no idea why it did not
/// stop, which is the most common complaint about autonomous loops.
pub fn decision_line(goal: &Goal, decision: &GateDecision) -> String {
    let reason = decision.reason();
    let round = goal.progress.rounds;
    match decision {
        GateDecision::Continue { wrap_up: true, .. } => {
            format!("goal: budget reached at round {round} — wrapping up")
        }
        GateDecision::Continue { .. } => format!("goal: not yet (round {round}) — {reason}"),
        GateDecision::Stop { status, .. } => match status {
            GoalStatus::Met => format!("goal: met after {round} round(s) — {reason}"),
            GoalStatus::Active => format!("goal: {reason}"),
            other => format!("goal: {} — {reason}", other.label()),
        },
    }
}

/// Whether the driver should keep running rounds after applying `decision`.
#[cfg(test)]
pub fn should_continue(goal: &Goal) -> bool {
    goal.status.is_running()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::goal::gate::{self, RoundSummary};
    use crate::extras::goal::{DEFAULT_RESTART_SUMMARY_CHARS, Report};
    use compact_str::CompactString;

    fn goal() -> Goal {
        Goal::new(
            "make the exporter idempotent",
            vec!["rerun is a no-op".into()],
        )
        .unwrap()
    }

    fn report(status: ReportStatus, round: u32, text: &str) -> Report {
        Report {
            status,
            evidence: Some(text.to_string()),
            blocker: Some(text.to_string()),
            reason: Some(text.to_string()),
            question: Some(text.to_string()),
            round,
            at: CompactString::new("now"),
        }
    }

    fn tool_call(name: &str) -> AgentEvent {
        AgentEvent::ToolCall {
            id: CompactString::new("1"),
            name: CompactString::new(name),
            args: serde_json::json!({}),
        }
    }

    #[test]
    fn the_collector_counts_mutating_calls_separately() {
        let mut collector = RoundCollector::new();
        collector.observe(&tool_call("read"));
        collector.observe(&tool_call("grep"));
        collector.observe(&tool_call("edit"));
        let summary = collector.finish(&goal(), RoundEnd::Done, 0);
        assert_eq!(summary.tool_calls, 3);
        assert_eq!(
            summary.mutating_tool_calls, 1,
            "only the edit can have changed the workspace"
        );
    }

    #[test]
    fn a_streamed_error_makes_the_round_a_failure_even_when_the_stream_closes() {
        let mut collector = RoundCollector::new();
        collector.observe(&AgentEvent::Error {
            message: CompactString::new("provider exploded"),
            interactions: Vec::new(),
        });
        let summary = collector.finish(&goal(), RoundEnd::Done, 0);
        assert_eq!(summary.end, RoundEnd::Failed("provider exploded".into()));
    }

    #[test]
    fn cancellation_is_preserved_over_a_streamed_error() {
        let mut collector = RoundCollector::new();
        collector.observe(&AgentEvent::Error {
            message: CompactString::new("late error"),
            interactions: Vec::new(),
        });
        let summary = collector.finish(&goal(), RoundEnd::Cancelled, 0);
        assert_eq!(summary.end, RoundEnd::Cancelled);
    }

    #[test]
    fn only_a_report_from_this_round_speaks_for_it() {
        let mut g = goal();
        g.progress.rounds = 2;
        g.push_report(report(ReportStatus::Met, 1, "stale claim"));
        let summary = RoundCollector::new().finish(&g, RoundEnd::Done, 0);
        assert!(
            summary.report.is_none(),
            "a report from round 1 must not decide round 3"
        );

        g.push_report(report(ReportStatus::Progress, 3, "fresh"));
        let summary = RoundCollector::new().finish(&g, RoundEnd::Done, 0);
        assert_eq!(
            summary.report.map(|r| r.status),
            Some(ReportStatus::Progress)
        );
    }

    #[test]
    fn time_spent_waiting_on_the_user_does_not_count_against_the_budget() {
        let mut collector = RoundCollector::new();
        collector.pause_clock();
        std::thread::sleep(Duration::from_millis(40));
        collector.resume_clock();
        let summary = collector.finish(&goal(), RoundEnd::Done, 0);
        assert_eq!(
            summary.active_secs, 0,
            "a permission prompt is not agent-active time"
        );
    }

    #[test]
    fn a_continue_round_keeps_the_conversation_and_states_the_round() {
        let g = goal();
        let decision = GateDecision::Continue {
            instruction: "keep going".into(),
            wrap_up: false,
            source: crate::extras::goal::VerdictSource::Structural,
        };
        let relaunch = next_round(&g, &decision).expect("continue relaunches");
        assert_eq!(relaunch.history, HistoryMode::Retained);
        assert!(relaunch.prompt.contains("Goal round 1 of 50"));
        assert!(relaunch.prompt.contains("keep going"));
        assert_eq!(relaunch.max_agent_turns, None);
        assert!(
            !relaunch.prompt.contains("make the exporter idempotent"),
            "a retained conversation already carries the objective in its preamble"
        );
    }

    #[test]
    fn a_restart_round_carries_the_objective_and_a_harness_built_summary() {
        let mut g = goal();
        g.continuation = ContinuationMode::Restart {
            summary_chars: DEFAULT_RESTART_SUMMARY_CHARS,
        };
        g.push_report(report(ReportStatus::Progress, 1, "wrote the migration"));
        g.progress.rounds = 1;

        let decision = GateDecision::Continue {
            instruction: "continue".into(),
            wrap_up: false,
            source: crate::extras::goal::VerdictSource::Structural,
        };
        let relaunch = next_round(&g, &decision).expect("continue relaunches");
        assert_eq!(relaunch.history, HistoryMode::Empty);
        assert!(
            relaunch.prompt.contains("make the exporter idempotent"),
            "a fresh run has no preamble memory of the objective"
        );
        assert!(relaunch.prompt.contains("rerun is a no-op"));
        assert!(relaunch.prompt.contains("wrote the migration"));
    }

    /// The summary is assembled from the record, so a model cannot use it to
    /// restate the objective as something easier on the way into the next round.
    #[test]
    fn the_carried_summary_is_built_from_the_record_and_bounded() {
        let mut g = goal();
        for round in 1..=6u32 {
            g.push_report(report(
                ReportStatus::Progress,
                round,
                &format!("did thing {round}"),
            ));
        }
        let summary = carried_summary(&g, 10_000).expect("reports present");
        assert!(summary.contains("did thing 6"));
        assert!(
            !summary.contains("did thing 1"),
            "only the recent rounds are carried"
        );

        let clipped = carried_summary(&g, 20).expect("reports present");
        assert!(
            clipped.chars().count() <= 21,
            "budget is honored: {clipped}"
        );
        assert_eq!(carried_summary(&g, 0), None);
    }

    #[test]
    fn the_wrap_up_round_is_capped_so_it_cannot_start_new_work() {
        let g = goal();
        let decision = GateDecision::Continue {
            instruction: "wrap up".into(),
            wrap_up: true,
            source: crate::extras::goal::VerdictSource::Bounds,
        };
        let relaunch = next_round(&g, &decision).expect("wrap-up still runs");
        assert_eq!(
            relaunch.max_agent_turns,
            Some(g.bounds.wrap_up_max_agent_turns)
        );
    }

    #[test]
    fn a_stop_decision_launches_nothing() {
        let g = goal();
        let decision = GateDecision::Stop {
            status: GoalStatus::Met,
            reason: "done".into(),
            paused_reason: None,
            source: crate::extras::goal::VerdictSource::Checks,
            evidence: Vec::new(),
        };
        assert_eq!(next_round(&g, &decision), None);
    }

    #[test]
    fn every_decision_produces_a_line_that_says_why() {
        let mut g = goal();
        g.progress.rounds = 3;

        let cont = GateDecision::Continue {
            instruction: "tests still failing".into(),
            wrap_up: false,
            source: crate::extras::goal::VerdictSource::Checks,
        };
        let line = decision_line(&g, &cont);
        assert!(line.contains("not yet"));
        assert!(line.contains("round 3"));
        assert!(line.contains("tests still failing"));

        let wrap = GateDecision::Continue {
            instruction: "wrap up".into(),
            wrap_up: true,
            source: crate::extras::goal::VerdictSource::Bounds,
        };
        assert!(decision_line(&g, &wrap).contains("wrapping up"));

        let met = GateDecision::Stop {
            status: GoalStatus::Met,
            reason: "cargo test passed".into(),
            paused_reason: None,
            source: crate::extras::goal::VerdictSource::Checks,
            evidence: Vec::new(),
        };
        assert!(decision_line(&g, &met).contains("met after 3 round(s)"));

        let blocked = GateDecision::Stop {
            status: GoalStatus::Blocked,
            reason: "no network".into(),
            paused_reason: None,
            source: crate::extras::goal::VerdictSource::ModelReport,
            evidence: Vec::new(),
        };
        assert!(decision_line(&g, &blocked).contains("blocked"));
    }

    /// A full scripted goal: two working rounds, then a verified completion.
    #[test]
    fn a_goal_runs_rounds_until_the_gate_stops_it() {
        let mut g = goal();
        g.judge = crate::extras::goal::JudgePolicy::Off;

        for round in 1..=2u32 {
            g.push_report(report(ReportStatus::Progress, round, "worked"));
            let summary = RoundSummary {
                mutating_tool_calls: 1,
                report: g.reports_in_round(round).last().cloned(),
                ..RoundSummary::completed()
            };
            let decision = match gate::gate_pre(&g, &summary) {
                gate::Step::Decided(d) => d,
                gate::Step::Verify(r) => gate::gate_post(&g, &summary, &r, None, None),
            };
            gate::apply(&mut g, &summary, &decision, None);
            assert!(should_continue(&g), "round {round} should continue");
            assert!(next_round(&g, &decision).is_some());
        }
        assert_eq!(g.progress.rounds, 2);

        g.push_report(report(ReportStatus::Met, 3, "all criteria satisfied"));
        let summary = RoundSummary {
            mutating_tool_calls: 1,
            report: g.reports_in_round(3).last().cloned(),
            ..RoundSummary::completed()
        };
        let decision = match gate::gate_pre(&g, &summary) {
            gate::Step::Decided(d) => d,
            gate::Step::Verify(r) => gate::gate_post(&g, &summary, &r, None, None),
        };
        gate::apply(&mut g, &summary, &decision, None);
        assert_eq!(g.status, GoalStatus::Met);
        assert!(!should_continue(&g));
        assert_eq!(next_round(&g, &decision), None);
    }
}

/// What the surface should do once a round has been judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundOutcome {
    /// No goal is running; behave exactly as before goals existed.
    Inactive,
    /// Launch another round. `line` says why, for the user watching.
    Relaunch { relaunch: Relaunch, line: String },
    /// Hand control back to the user.
    Stopped { status: GoalStatus, line: String },
}

/// Judge the round that just ended and record the result.
///
/// This is the one place a round is folded into the goal, so the TUI, headless
/// and ACP surfaces cannot drift apart on what a round meant. Verification
/// tiers are supplied by the caller, which is what keeps this free of I/O: the
/// checks and judge land in later slices and plug in here.
pub async fn settle_round<F, Fut>(
    store: &super::GoalStore,
    summary: RoundSummary,
    run_verification: F,
) -> RoundOutcome
where
    F: FnOnce(super::gate::VerifyRequest) -> Fut,
    Fut: std::future::Future<
            Output = (
                Option<super::gate::CheckOutcome>,
                Option<super::gate::JudgeOutcome>,
            ),
        >,
{
    let Some(goal) = store.snapshot() else {
        return RoundOutcome::Inactive;
    };
    if goal.status.is_terminal() {
        return RoundOutcome::Inactive;
    }

    let (decision, checked, judged) = match super::gate::gate_pre(&goal, &summary) {
        super::gate::Step::Decided(decision) => (decision, None, None),
        super::gate::Step::Verify(request) => {
            let (checks, judge) = run_verification(request.clone()).await;
            let decision =
                super::gate::gate_post(&goal, &summary, &request, checks.as_ref(), judge.as_ref());
            (decision, checks, judge)
        }
    };

    let line = store
        .with_mut(|goal| {
            super::gate::apply(goal, &summary, &decision, judged.as_ref());
            // Written after the fold so the record shows the status the round
            // actually produced.
            super::transcript::save_round(
                goal,
                &summary,
                &decision,
                decision.reason(),
                checked.as_ref(),
                judged.as_ref(),
            );
            #[cfg(feature = "hooks")]
            super::publish_hook_info(Some(goal));
            decision_line(goal, &decision)
        })
        .unwrap_or_default();

    let Some(goal) = store.snapshot() else {
        return RoundOutcome::Inactive;
    };
    match next_round(&goal, &decision) {
        Some(relaunch) if goal.status.is_running() => RoundOutcome::Relaunch { relaunch, line },
        _ => RoundOutcome::Stopped {
            status: goal.status,
            line,
        },
    }
}

/// Verification hook for tests and for surfaces with no tiers wired.
#[cfg(test)]
pub async fn no_verification(
    _request: super::gate::VerifyRequest,
) -> (
    Option<super::gate::CheckOutcome>,
    Option<super::gate::JudgeOutcome>,
) {
    (None, None)
}

#[cfg(test)]
mod settle_tests {
    use super::*;
    use crate::extras::goal::{Goal, GoalStore, JudgePolicy, ReportStatus};

    /// Redirect the application data root so a settled round writes its
    /// transcript into a temporary directory instead of the real one.
    struct IsolatedPaths {
        path: std::path::PathBuf,
        _environment: crate::tests::ScopedProcessEnv,
    }

    impl Drop for IsolatedPaths {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn isolated_paths() -> IsolatedPaths {
        let path =
            std::env::temp_dir().join(format!("zerostack-goal-tests-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        let environment = crate::tests::ScopedProcessEnv::set(&[(
            "ZS_DATA_DIR",
            Some(path.as_os_str().to_os_string()),
        )]);
        IsolatedPaths {
            path,
            _environment: environment,
        }
    }

    fn store() -> GoalStore {
        let store = GoalStore::default();
        let mut goal = Goal::new("finish the job", Vec::new()).unwrap();
        goal.judge = JudgePolicy::Off;
        store.set(goal, false).unwrap();
        store
    }

    fn report(store: &GoalStore, status: ReportStatus) {
        let round = store.snapshot().unwrap().progress.rounds + 1;
        store.append_report(crate::extras::goal::Report {
            status,
            evidence: Some("did it".into()),
            blocker: Some("stuck".into()),
            reason: Some("cannot".into()),
            question: Some("which one?".into()),
            round,
            at: compact_str::CompactString::new("now"),
        });
    }

    #[tokio::test]
    async fn a_working_round_relaunches_with_a_visible_reason() {
        let _paths = isolated_paths();
        let store = store();
        report(&store, ReportStatus::Progress);
        let summary = RoundSummary {
            mutating_tool_calls: 1,
            report: store.snapshot().unwrap().last_report().cloned(),
            ..RoundSummary::completed()
        };
        match settle_round(&store, summary, no_verification).await {
            RoundOutcome::Relaunch { relaunch, line } => {
                assert!(line.contains("not yet"));
                assert!(relaunch.prompt.contains("Goal round 2"));
            }
            other => panic!("expected a relaunch, got {other:?}"),
        }
        assert_eq!(store.snapshot().unwrap().progress.rounds, 1);
    }

    #[tokio::test]
    async fn a_verified_completion_stops_and_records_the_status() {
        let _paths = isolated_paths();
        let store = store();
        report(&store, ReportStatus::Met);
        let summary = RoundSummary {
            mutating_tool_calls: 1,
            report: store.snapshot().unwrap().last_report().cloned(),
            ..RoundSummary::completed()
        };
        match settle_round(&store, summary, no_verification).await {
            RoundOutcome::Stopped { status, line } => {
                assert_eq!(status, GoalStatus::Met);
                assert!(line.contains("met after 1 round"));
            }
            other => panic!("expected a stop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn with_no_goal_the_surface_behaves_as_if_goals_did_not_exist() {
        let _paths = isolated_paths();
        let empty = GoalStore::default();
        let outcome = settle_round(&empty, RoundSummary::completed(), no_verification).await;
        assert_eq!(outcome, RoundOutcome::Inactive);
    }

    #[tokio::test]
    async fn an_interrupted_round_stops_without_consuming_the_goal() {
        let _paths = isolated_paths();
        let store = store();
        let summary = RoundSummary {
            end: RoundEnd::Cancelled,
            ..RoundSummary::completed()
        };
        match settle_round(&store, summary, no_verification).await {
            RoundOutcome::Stopped { status, .. } => assert_eq!(status, GoalStatus::Active),
            other => panic!("expected an interrupt stop, got {other:?}"),
        }
        let goal = store.snapshot().unwrap();
        assert_eq!(goal.progress.rounds, 0);
        assert_eq!(goal.status, GoalStatus::Active);
    }

    /// The verification hook is only consulted when the gate asks for it, so an
    /// ordinary round never pays for checks or a judge.
    #[tokio::test]
    async fn verification_runs_only_on_a_completion_claim() {
        let _paths = isolated_paths();
        let store = store();
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let flag = called.clone();
        report(&store, ReportStatus::Progress);
        let summary = RoundSummary {
            mutating_tool_calls: 1,
            report: store.snapshot().unwrap().last_report().cloned(),
            ..RoundSummary::completed()
        };
        settle_round(&store, summary, move |_| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            async { (None, None) }
        })
        .await;
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "an ordinary round must not run verification"
        );

        let flag = called.clone();
        report(&store, ReportStatus::Met);
        let summary = RoundSummary {
            mutating_tool_calls: 1,
            report: store.snapshot().unwrap().last_report().cloned(),
            ..RoundSummary::completed()
        };
        settle_round(&store, summary, move |_| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            async { (None, None) }
        })
        .await;
        assert!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            "a completion claim must be verified"
        );
    }
}

/// Build a round summary from a completed headless turn.
///
/// The headless driver gets a finished [`crate::agent::runner::HeadlessTurn`]
/// rather than a live event stream, so the same facts are recovered from the
/// turn's own record. Both paths end up at the identical [`RoundSummary`], which
/// is what keeps a goal behaving the same way in a terminal and in CI.
pub fn summary_from_headless_turn(
    goal: &Goal,
    interactions: &[rig::completion::Message],
    usage: &rig::completion::Usage,
    failure: Option<&anyhow::Error>,
    open_todos: usize,
    verify_configured: bool,
    active: Duration,
) -> RoundSummary {
    use rig::message::{AssistantContent, Message};

    let mut tool_calls = 0u32;
    let mut mutating_tool_calls = 0u32;
    for message in interactions {
        let Message::Assistant { content, .. } = message else {
            continue;
        };
        for item in content.iter() {
            if let AssistantContent::ToolCall(call) = item {
                tool_calls += 1;
                if crate::agent::runner::tool_may_mutate_workspace(&call.function.name) {
                    mutating_tool_calls += 1;
                }
            }
        }
    }

    let round = goal.progress.rounds + 1;
    RoundSummary {
        end: match failure {
            Some(error) => RoundEnd::Failed(error.to_string()),
            None => RoundEnd::Done,
        },
        tool_calls,
        mutating_tool_calls,
        report: goal.reports_in_round(round).last().cloned(),
        // Headless verification status is reported on stderr by the runner
        // rather than through an event the driver can see; a completion claim
        // therefore re-runs the command through the checks tier.
        verify_ran: false,
        verify_passed: None,
        verify_configured,
        open_todos,
        tokens_used: usage.input_tokens + usage.output_tokens,
        active_secs: active.as_secs(),
    }
}
