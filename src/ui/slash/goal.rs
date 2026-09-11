//! `/goal` — set, inspect, and steer the session's objective.
//!
//! Owning specification: `docs/superpowers/specs/2026-09-11-goal-feature-design.md`
//! §4.7 (surfaces).

use crate::extras::goal::{Goal, GoalStatus};
use crate::ui::slash::{SlashCtx, write_error, write_ok, write_result};

const USAGE: &str = "usage: /goal <objective>  |  /goal status | pause | resume | reopen | clear \
| bounds <key>=<value>";

/// Split a `/goal` body into its objective and any `done when:` criteria.
///
/// Criteria are optional and come from lines after a `done when:` marker, so a
/// user can state acceptance up front in the same command instead of
/// discovering later that "done" meant something else.
pub(crate) fn parse_objective(body: &str) -> (String, Vec<String>) {
    let mut objective = Vec::new();
    let mut criteria = Vec::new();
    let mut in_criteria = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if !in_criteria && trimmed.eq_ignore_ascii_case("done when:") {
            in_criteria = true;
            continue;
        }
        if in_criteria {
            let item = trimmed.trim_start_matches(['-', '*', '•']).trim();
            if !item.is_empty() {
                criteria.push(item.to_string());
            }
        } else if !trimmed.is_empty() {
            objective.push(trimmed);
        }
    }
    (objective.join(" "), criteria)
}

fn parse_bound(goal: &mut Goal, assignment: &str) -> Result<String, String> {
    let (key, value) = assignment
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got {assignment:?}"))?;
    let key = key.trim();
    let value = value.trim();

    let number = |v: &str| -> Result<u64, String> {
        v.parse::<u64>()
            .map_err(|_| format!("{key} needs a whole number, got {v:?}"))
    };

    match key {
        "max_rounds" => {
            let parsed = number(value)?.clamp(1, 10_000) as u32;
            goal.bounds.max_rounds = parsed;
            Ok(format!("max_rounds = {parsed}"))
        }
        "max_tokens" => {
            let parsed = number(value)?;
            goal.bounds.max_tokens = (parsed > 0).then_some(parsed);
            Ok(format!("max_tokens = {parsed}"))
        }
        "max_active_secs" => {
            let parsed = number(value)?;
            goal.bounds.max_active_secs = (parsed > 0).then_some(parsed);
            Ok(format!("max_active_secs = {parsed}"))
        }
        "no_progress_rounds" => {
            let parsed = number(value)?.clamp(1, 100) as u32;
            goal.bounds.no_progress_rounds = parsed;
            Ok(format!("no_progress_rounds = {parsed}"))
        }
        "blocked_rounds" => {
            let parsed = number(value)?.clamp(1, 100) as u32;
            goal.bounds.blocked_rounds = parsed;
            Ok(format!("blocked_rounds = {parsed}"))
        }
        "wrap_up_max_agent_turns" => {
            let parsed = number(value)?.clamp(1, 100) as u32;
            goal.bounds.wrap_up_max_agent_turns = parsed;
            Ok(format!("wrap_up_max_agent_turns = {parsed}"))
        }
        "reinject_every" => {
            let parsed = number(value)?.min(1_000) as u32;
            goal.bounds.reinject_every = parsed;
            Ok(format!("reinject_every = {parsed}"))
        }
        "judge_every" => {
            let parsed = number(value)?.min(1_000) as u32;
            goal.bounds.judge_every = parsed;
            Ok(format!("judge_every = {parsed}"))
        }
        "continuation" => match value {
            "continue" => {
                goal.continuation = crate::extras::goal::ContinuationMode::Continue;
                Ok("continuation = continue".into())
            }
            "restart" => {
                goal.continuation = crate::extras::goal::ContinuationMode::Restart {
                    summary_chars: crate::extras::goal::DEFAULT_RESTART_SUMMARY_CHARS,
                };
                Ok("continuation = restart".into())
            }
            other => Err(format!(
                "continuation must be continue or restart, got {other:?}"
            )),
        },
        other => Err(format!("unknown bound {other:?}")),
    }
}

/// Human-readable status, used by `/goal status` and the headless summary.
pub(crate) fn render_status(goal: &Goal) -> String {
    let mut out = String::new();
    out.push_str(&format!("objective: {}\n", goal.objective));
    if !goal.criteria.is_empty() {
        out.push_str("done when:\n");
        for criterion in &goal.criteria {
            out.push_str(&format!("  - {criterion}\n"));
        }
    }
    out.push_str(&format!("status: {}", goal.status.label()));
    if let Some(reason) = goal.paused_reason {
        out.push_str(&format!(" ({})", reason.describe()));
    }
    out.push('\n');
    out.push_str(&format!(
        "round: {} of {}\n",
        goal.progress.rounds, goal.bounds.max_rounds
    ));
    out.push_str(&format!("tokens: {}\n", goal.progress.tokens_used));
    if goal.progress.active_secs > 0 {
        out.push_str(&format!("active: {}s\n", goal.progress.active_secs));
    }
    out.push_str(&format!(
        "verification: {}\n",
        if goal.checks.is_empty() {
            "self-report only (add /goal check <command> to verify)".to_string()
        } else {
            format!("{} check(s)", goal.checks.len())
        }
    ));
    out.push_str(&format!(
        "judge: {}\n",
        goal.resolved_judge
            .as_ref()
            .map(|j| j.describe())
            .unwrap_or_else(|| match goal.judge {
                crate::extras::goal::JudgePolicy::Off => "off".to_string(),
                _ => "resolved when the next round runs".to_string(),
            })
    ));
    if let Some(verdict) = &goal.last_verdict {
        out.push_str(&format!("last check: {}\n", verdict.reason));
        if goal.met_unverified() {
            out.push_str("note: completion was not proved by any command\n");
        }
    }
    out
}

pub(crate) async fn handle_goal(parts: &[&str], body: &str, ctx: &mut SlashCtx<'_>) {
    let store = ctx.session.goal_store.clone();
    let subcommand = parts.get(1).copied().unwrap_or("status");

    match subcommand {
        "status" if parts.len() <= 2 => match store.snapshot() {
            Some(goal) => write_result(ctx.renderer, render_status(&goal)),
            None => write_result(ctx.renderer, "no goal set — /goal <objective> to set one"),
        },
        "clear" => match store.clear() {
            Some(goal) => {
                write_ok(ctx.renderer, format!("goal cleared: {}", goal.objective));
                ctx.rebuild_agent().await;
            }
            None => write_result(ctx.renderer, "no goal to clear"),
        },
        "pause" => {
            let changed = store.with_mut(|goal| {
                goal.set_status(GoalStatus::Paused, None);
                goal.status
            });
            match changed {
                Some(status) => write_ok(ctx.renderer, format!("goal {}", status.label())),
                None => write_error(ctx.renderer, "no goal to pause"),
            }
        }
        "resume" => match store.with_mut(Goal::resume) {
            Some(true) => write_ok(ctx.renderer, "goal resumed"),
            Some(false) => write_error(ctx.renderer, "a finished goal cannot resume; /goal reopen"),
            None => write_error(ctx.renderer, "no goal to resume"),
        },
        "reopen" => match store.with_mut(Goal::reopen) {
            Some(true) => {
                write_ok(ctx.renderer, "goal reopened");
                ctx.rebuild_agent().await;
            }
            Some(false) => write_error(ctx.renderer, "only a goal marked impossible can reopen"),
            None => write_error(ctx.renderer, "no goal to reopen"),
        },
        "bounds" => {
            if parts.len() < 3 {
                write_error(ctx.renderer, "usage: /goal bounds <key>=<value>");
                return;
            }
            let mut applied = Vec::new();
            let mut failed = Vec::new();
            store.with_mut(|goal| {
                for assignment in &parts[2..] {
                    match parse_bound(goal, assignment) {
                        Ok(message) => applied.push(message),
                        Err(message) => failed.push(message),
                    }
                }
                // Raising a bound is how a user answers a budget stop, so the
                // wrap-up is re-armed for the new budget.
                if !applied.is_empty() && goal.status == GoalStatus::BudgetLimited {
                    goal.resume();
                }
            });
            if store.is_empty() {
                write_error(ctx.renderer, "no goal to configure");
                return;
            }
            for message in failed {
                write_error(ctx.renderer, message);
            }
            if !applied.is_empty() {
                write_ok(ctx.renderer, applied.join(", "));
            }
        }
        "check" => {
            if parts.len() < 3 {
                write_error(
                    ctx.renderer,
                    "usage: /goal check <command>  |  /goal check clear",
                );
                return;
            }
            let command = body
                .trim()
                .strip_prefix("check")
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            let outcome = store.with_mut(|goal| {
                if command == "clear" {
                    goal.checks.clear();
                    "checks cleared".to_string()
                } else {
                    goal.checks
                        .push(crate::extras::goal::GoalCheck::new(command.clone()));
                    format!("check added: {command}")
                }
            });
            match outcome {
                Some(message) => write_ok(ctx.renderer, message),
                None => write_error(ctx.renderer, "no goal to add a check to"),
            }
        }
        _ => {
            if body.trim().is_empty() {
                write_error(ctx.renderer, USAGE);
                return;
            }
            let (objective, criteria) = parse_objective(body);
            let goal = match Goal::new(objective, criteria) {
                Ok(goal) => goal,
                Err(error) => {
                    write_error(ctx.renderer, error);
                    return;
                }
            };
            // Replacing a live goal is refused rather than done silently: the
            // progress, verdicts, and reports on it are the record of work.
            if let Err(error) = store.set(goal, false) {
                write_error(ctx.renderer, error);
                if let Some(existing) = store.snapshot() {
                    write_result(ctx.renderer, render_status(&existing));
                }
                return;
            }
            let summary = store
                .snapshot()
                .map(|g| g.objective.clone())
                .unwrap_or_default();
            write_ok(ctx.renderer, format!("goal set: {summary}"));
            ctx.rebuild_agent().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn criteria_are_parsed_from_a_done_when_section() {
        let (objective, criteria) = parse_objective(
            "retire the legacy exporter\ndone when:\n- no callers remain\n* the tests pass",
        );
        assert_eq!(objective, "retire the legacy exporter");
        assert_eq!(criteria, vec!["no callers remain", "the tests pass"]);
    }

    #[test]
    fn an_objective_without_criteria_stays_whole() {
        let (objective, criteria) = parse_objective("make   the build\nreproducible");
        assert_eq!(objective, "make   the build reproducible");
        assert!(criteria.is_empty());
    }

    #[test]
    fn bounds_are_parsed_clamped_and_validated() {
        let mut goal = Goal::new("x", Vec::new()).unwrap();

        assert!(parse_bound(&mut goal, "max_rounds=7").is_ok());
        assert_eq!(goal.bounds.max_rounds, 7);

        assert!(parse_bound(&mut goal, "max_rounds=0").is_ok());
        assert_eq!(goal.bounds.max_rounds, 1, "clamped to a runnable minimum");

        assert!(parse_bound(&mut goal, "max_tokens=50000").is_ok());
        assert_eq!(goal.bounds.max_tokens, Some(50_000));
        assert!(parse_bound(&mut goal, "max_tokens=0").is_ok());
        assert_eq!(goal.bounds.max_tokens, None, "zero means unbounded");

        assert!(parse_bound(&mut goal, "continuation=restart").is_ok());
        assert!(matches!(
            goal.continuation,
            crate::extras::goal::ContinuationMode::Restart { .. }
        ));

        assert!(parse_bound(&mut goal, "continuation=sideways").is_err());
        assert!(parse_bound(&mut goal, "max_rounds=many").is_err());
        assert!(parse_bound(&mut goal, "nonsense=1").is_err());
        assert!(parse_bound(&mut goal, "max_rounds").is_err());
    }

    #[test]
    fn status_reports_what_the_completion_actually_rests_on() {
        let mut goal = Goal::new("ship it", vec!["tests pass".into()]).unwrap();
        let rendered = render_status(&goal);
        assert!(rendered.contains("objective: ship it"));
        assert!(rendered.contains("done when:"));
        assert!(
            rendered.contains("self-report only"),
            "a goal with no checks must say so: {rendered}"
        );

        goal.checks
            .push(crate::extras::goal::GoalCheck::new("cargo test"));
        assert!(render_status(&goal).contains("1 check(s)"));

        goal.set_status(
            GoalStatus::Paused,
            Some(crate::extras::goal::PauseReason::NoProgress),
        );
        let paused = render_status(&goal);
        assert!(paused.contains("paused"));
        assert!(
            paused.contains("no progress"),
            "the pause reason must be visible: {paused}"
        );
    }

    #[test]
    fn an_unverified_completion_is_labelled_in_the_status() {
        let mut goal = Goal::new("ship it", Vec::new()).unwrap();
        goal.set_status(GoalStatus::Met, None);
        goal.last_verdict = Some(crate::extras::goal::Verdict {
            outcome: crate::extras::goal::Outcome::Met,
            reason: "the model said so".into(),
            source: crate::extras::goal::VerdictSource::ModelReport,
            evidence: vec![crate::extras::goal::VerificationKind::SelfReport],
            at: compact_str::CompactString::new("now"),
        });
        assert!(render_status(&goal).contains("not proved by any command"));
    }
}
