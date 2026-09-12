//! Building a goal from a `--loop` configuration.
//!
//! `--loop` and goals both drive an agent over repeated rounds, and most of a
//! loop's configuration maps onto a goal exactly. This module is that mapping,
//! written down and tested so the two features cannot drift apart on what a
//! round means.
//!
//! A loop validator runs after every iteration and feeds the next prompt;
//! a goal check by default runs only on a completion claim and gates it. Those
//! were different contracts until goals learned to run checks every round, so
//! the preset sets `check_every_round` whenever a validator is given. That is
//! what makes the mapping faithful rather than a silent change of meaning for
//! everyone already using `--loop-run`.
//!
//! Owning specification: `docs/specs/goals.md` (Round model).

use std::path::{Path, PathBuf};

use super::{
    ContinuationMode, DEFAULT_RESTART_SUMMARY_CHARS, Goal, GoalCheck, GoalDefaults, GoalError,
    JudgePolicy,
};

/// What a loop configuration becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopPreset {
    pub goal: Goal,
    /// The plan file the loop reads and asks the agent to maintain.
    pub plan_file: PathBuf,
    /// Behaviour the goal model cannot express. Empty means the mapping is
    /// faithful; a non-empty entry would mean the fold changes meaning and must
    /// not be performed.
    pub unmapped: Vec<&'static str>,
}

/// Build the goal equivalent to a loop configuration.
///
/// Installation configuration is folded in first and the loop's own settings
/// are applied over it, so `--loop-max` wins over a `[goal]` table while a
/// project's `goal_checks` still apply.
pub fn loop_goal(
    prompt: &str,
    plan_file: &Path,
    max_iterations: Option<u32>,
    run_cmd: Option<&str>,
    defaults: GoalDefaults<'_>,
) -> Result<LoopPreset, GoalError> {
    let mut goal = Goal::configured(prompt, Vec::new(), defaults)?;

    // A loop has never called a second model, and its early finish is meant to
    // rest on the validator rather than on a reading of the transcript. So the
    // judge stays off unless this installation named one, in which case the
    // user asked for the review and gets it.
    if !defaults.cfg.goal_judge_configured() {
        goal.judge = JudgePolicy::Off;
        goal.resolved_judge = None;
    }

    // A loop starts each iteration from a clean conversation and carries a
    // summary forward, which is exactly the restart mode.
    goal.continuation = ContinuationMode::Restart {
        summary_chars: super::super::r#loop::SUMMARY_TRUNCATION_CHARS
            .min(DEFAULT_RESTART_SUMMARY_CHARS.max(super::super::r#loop::SUMMARY_TRUNCATION_CHARS)),
    };
    // `--loop-max 0` means "run nothing", which a goal's minimum of one round
    // cannot express; the caller answers it before a goal is built. Anything
    // that reaches here runs at least one iteration.
    if let Some(max) = max_iterations {
        goal.bounds.max_rounds = max.max(1);
    }
    // `--loop-max N` runs exactly N iterations; a goal's wind-down round would
    // silently make it N + 1.
    goal.bounds.wrap_up_max_agent_turns = 0;

    // The plan file travels with the goal and is re-read every round, which is
    // what the loop's own prompt did.
    goal.context_file = Some(plan_file.to_path_buf());

    let unmapped = Vec::new();
    if let Some(command) = run_cmd.map(str::trim).filter(|c| !c.is_empty()) {
        goal.checks.push(GoalCheck::new(command));
        // A loop validator is per-iteration feedback, not a completion gate.
        goal.bounds.check_every_round = true;
    }

    Ok(LoopPreset {
        goal,
        plan_file: plan_file.to_path_buf(),
        unmapped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> crate::config::Config {
        crate::config::Config::default()
    }

    fn defaults(cfg: &crate::config::Config) -> GoalDefaults<'_> {
        GoalDefaults {
            cfg,
            provider: "openrouter",
            model: "big",
        }
    }

    #[test]
    fn a_loop_prompt_and_cap_map_onto_a_goal() {
        let preset = loop_goal(
            "keep improving the parser",
            Path::new("LOOP_PLAN.md"),
            Some(7),
            None,
            defaults(&config()),
        )
        .expect("valid preset");
        assert_eq!(preset.goal.objective, "keep improving the parser");
        assert_eq!(preset.goal.bounds.max_rounds, 7);
        assert_eq!(
            preset.goal.bounds.wrap_up_max_agent_turns, 0,
            "an iteration cap is exact, not a cap plus a wind-down"
        );
        assert!(
            matches!(preset.goal.continuation, ContinuationMode::Restart { .. }),
            "a loop iteration starts from a clean conversation"
        );
        assert_eq!(preset.plan_file, Path::new("LOOP_PLAN.md"));
        assert!(
            preset.unmapped.is_empty(),
            "without a validator the mapping is complete"
        );
    }

    /// A loop has never called a second model, and its early finish is meant
    /// to rest on the validator. So the judge stays off unless the
    /// installation named one, in which case the user asked for the review.
    #[test]
    fn a_loop_does_not_acquire_a_judge_nobody_asked_for() {
        let plain = loop_goal(
            "work",
            Path::new("LOOP_PLAN.md"),
            None,
            None,
            defaults(&config()),
        )
        .expect("valid preset");
        assert_eq!(plain.goal.judge, JudgePolicy::Off);
        assert!(plain.goal.resolved_judge.is_none());

        let mut cfg = config();
        cfg.goal = Some(Box::new(crate::config::types::GoalConfig {
            judge: Some("session".to_string()),
            ..Default::default()
        }));
        let asked_for = loop_goal(
            "work",
            Path::new("LOOP_PLAN.md"),
            None,
            None,
            defaults(&cfg),
        )
        .expect("valid preset");
        assert_eq!(asked_for.goal.judge, JudgePolicy::Session);
    }

    /// Configuration is the floor and the flags are the override: a project's
    /// required check still applies to a loop, while `--loop-max` beats a
    /// `[goal]` round cap and the iteration count stays exact.
    #[test]
    fn loop_flags_win_over_configuration_without_discarding_it() {
        let mut cfg = config();
        cfg.goal_checks = Some(vec!["cargo fmt --check".into()]);
        cfg.goal = Some(Box::new(crate::config::types::GoalConfig {
            max_rounds: Some(99),
            wrap_up_max_agent_turns: Some(4),
            ..Default::default()
        }));

        let preset = loop_goal(
            "work",
            Path::new("LOOP_PLAN.md"),
            Some(3),
            Some("cargo test"),
            defaults(&cfg),
        )
        .expect("valid preset");

        assert_eq!(preset.goal.bounds.max_rounds, 3, "--loop-max wins");
        assert_eq!(
            preset.goal.bounds.wrap_up_max_agent_turns, 0,
            "an iteration cap stays exact whatever the table says"
        );
        assert_eq!(
            preset
                .goal
                .checks
                .iter()
                .map(|c| c.command.as_str())
                .collect::<Vec<_>>(),
            ["cargo fmt --check", "cargo test"],
            "the project's check survives alongside the validator"
        );
    }

    /// A loop validator is feedback after every iteration, so the preset must
    /// ask for per-round checks rather than a completion gate. Getting this
    /// wrong would change what --loop-run means for everyone already using it.
    #[test]
    fn a_loop_validator_becomes_a_per_round_check() {
        let preset = loop_goal(
            "work",
            Path::new("LOOP_PLAN.md"),
            None,
            Some("cargo test"),
            defaults(&config()),
        )
        .expect("valid preset");
        assert_eq!(preset.goal.checks.len(), 1);
        assert_eq!(preset.goal.checks[0].command, "cargo test");
        assert!(
            preset.goal.bounds.check_every_round,
            "a loop validator runs every iteration, not only at the end"
        );
        assert!(
            preset.unmapped.is_empty(),
            "with per-round checks the mapping is faithful"
        );
    }

    #[test]
    fn the_plan_file_travels_with_the_goal() {
        let preset = loop_goal(
            "work",
            Path::new("LOOP_PLAN.md"),
            None,
            None,
            defaults(&config()),
        )
        .expect("valid preset");
        assert_eq!(
            preset.goal.context_file.as_deref(),
            Some(Path::new("LOOP_PLAN.md")),
            "the plan must be re-read every round, as the loop prompt did"
        );
    }

    #[test]
    fn an_empty_validator_is_not_a_check() {
        let preset = loop_goal(
            "work",
            Path::new("LOOP_PLAN.md"),
            None,
            Some("   "),
            defaults(&config()),
        )
        .expect("valid preset");
        assert!(preset.goal.checks.is_empty());
        assert!(!preset.goal.bounds.check_every_round);
        assert!(preset.unmapped.is_empty());
    }

    #[test]
    fn a_zero_cap_still_runs_one_round() {
        let preset = loop_goal(
            "work",
            Path::new("LOOP_PLAN.md"),
            Some(0),
            None,
            defaults(&config()),
        )
        .expect("valid preset");
        assert_eq!(preset.goal.bounds.max_rounds, 1);
    }

    #[test]
    fn an_empty_prompt_is_refused_the_same_way_any_objective_is() {
        assert_eq!(
            loop_goal(
                "  ",
                Path::new("LOOP_PLAN.md"),
                None,
                None,
                defaults(&config()),
            )
            .unwrap_err(),
            GoalError::EmptyObjective
        );
    }
}
