//! Building a goal from a `--loop` configuration.
//!
//! `--loop` and goals both drive an agent over repeated rounds, and most of a
//! loop's configuration maps onto a goal exactly. This module is that mapping,
//! written down and tested so the two features cannot drift apart on what a
//! round means.
//!
//! One thing does **not** map, and the mapping refuses to pretend otherwise.
//! `--loop-run` executes after every iteration and its output is fed into the
//! next prompt as feedback; goal checks run only when the agent claims
//! completion, and gate it. Those are different contracts, so [`loop_goal`]
//! carries the validator as a check and reports that the per-iteration
//! feedback behaviour is not represented. Folding `--loop` onto goals without
//! closing that gap would silently turn a feedback signal into a completion
//! gate for everyone already using it.
//!
//! Owning specification: `docs/specs/goals.md` (Round model).

#[cfg(test)]
use std::path::{Path, PathBuf};

#[cfg(test)]
use super::{ContinuationMode, DEFAULT_RESTART_SUMMARY_CHARS, Goal, GoalCheck, GoalError};

/// What a loop configuration becomes, and what it loses on the way.
///
/// Not yet consumed by `--loop` itself: the fold is blocked on the unmapped
/// behaviour this type reports (`mini-agent-a1qwa.17`). It exists now so the
/// mapping is written down and tested rather than rediscovered later.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopPreset {
    pub goal: Goal,
    /// The plan file the loop reads and asks the agent to maintain.
    pub plan_file: PathBuf,
    /// Behaviour the goal model cannot express yet. Non-empty means the fold is
    /// not faithful and must not be performed.
    pub unmapped: Vec<&'static str>,
}

/// Build the goal equivalent to a loop configuration.
#[cfg(test)]
pub fn loop_goal(
    prompt: &str,
    plan_file: &Path,
    max_iterations: Option<u32>,
    run_cmd: Option<&str>,
) -> Result<LoopPreset, GoalError> {
    let mut goal = Goal::new(prompt, Vec::new())?;

    // A loop starts each iteration from a clean conversation and carries a
    // summary forward, which is exactly the restart mode.
    goal.continuation = ContinuationMode::Restart {
        summary_chars: super::super::r#loop::SUMMARY_TRUNCATION_CHARS
            .min(DEFAULT_RESTART_SUMMARY_CHARS.max(super::super::r#loop::SUMMARY_TRUNCATION_CHARS)),
    };
    if let Some(max) = max_iterations {
        goal.bounds.max_rounds = max.max(1);
    }

    let mut unmapped = Vec::new();
    if let Some(command) = run_cmd.map(str::trim).filter(|c| !c.is_empty()) {
        goal.checks.push(GoalCheck::new(command));
        unmapped.push(
            "--loop-run executes after every iteration and feeds its output into the next \
             prompt; a goal check runs only on a completion claim and gates it",
        );
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

    #[test]
    fn a_loop_prompt_and_cap_map_onto_a_goal() {
        let preset = loop_goal(
            "keep improving the parser",
            Path::new("LOOP_PLAN.md"),
            Some(7),
            None,
        )
        .expect("valid preset");
        assert_eq!(preset.goal.objective, "keep improving the parser");
        assert_eq!(preset.goal.bounds.max_rounds, 7);
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

    /// The gap that stops the fold. A loop validator is feedback after every
    /// iteration; a goal check is a completion gate. Silently swapping one for
    /// the other would change behaviour for everyone already using `--loop-run`.
    #[test]
    fn a_loop_validator_does_not_map_onto_a_goal_check() {
        let preset = loop_goal("work", Path::new("LOOP_PLAN.md"), None, Some("cargo test"))
            .expect("valid preset");
        assert_eq!(preset.goal.checks.len(), 1);
        assert_eq!(preset.goal.checks[0].command, "cargo test");
        assert_eq!(
            preset.unmapped.len(),
            1,
            "the per-iteration feedback contract must be reported as unmapped"
        );
        assert!(preset.unmapped[0].contains("after every iteration"));
    }

    #[test]
    fn an_empty_validator_is_not_a_check() {
        let preset =
            loop_goal("work", Path::new("LOOP_PLAN.md"), None, Some("   ")).expect("valid preset");
        assert!(preset.goal.checks.is_empty());
        assert!(preset.unmapped.is_empty());
    }

    #[test]
    fn a_zero_cap_still_runs_one_round() {
        let preset =
            loop_goal("work", Path::new("LOOP_PLAN.md"), Some(0), None).expect("valid preset");
        assert_eq!(preset.goal.bounds.max_rounds, 1);
    }

    #[test]
    fn an_empty_prompt_is_refused_the_same_way_any_objective_is() {
        assert_eq!(
            loop_goal("  ", Path::new("LOOP_PLAN.md"), None, None).unwrap_err(),
            GoalError::EmptyObjective
        );
    }
}
