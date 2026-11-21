//! Proving a completion claim with commands.
//!
//! This is the only tier that produces external evidence. A model saying it is
//! finished and a judge agreeing are both accounts of the work; a command
//! exiting zero is a fact about it. That is why a failing check outranks every
//! other signal, and why a goal whose checks cannot run can never reach
//! completion through this tier.
//!
//! Checks reach the same bounded, sandboxed runner the loop validator uses
//! (`TC-GOAL-CHECK` in `docs/specs/subprocess-trust.md`). The goal module never
//! builds a process itself and never calls `Sandbox::wrap_command`, so the
//! existing containment, timeout, and output limits apply unchanged.
//!
//! Owning specification: `docs/specs/goals.md` (Verification tiers).

use super::gate::{CheckFailureKind, CheckOutcome, VerifyRequest};
use super::{Goal, VerificationKind};
use crate::sandbox::{CommandLimits, Sandbox};

/// How much of a failing command's output is fed back to the model.
const FAILURE_TAIL_CHARS: usize = 4_000;

/// Limits for one check.
///
/// The completion-verification gate's own bounds, not a copy of them: a goal
/// check and a `verify_command` are the same kind of thing, and a second set of
/// numbers would eventually disagree with the first without anyone noticing.
fn limits(cfg: &crate::config::Config) -> CommandLimits {
    crate::agent::runner::CompletionVerification::limits(cfg)
}

/// Run everything the gate asked for, in order.
///
/// The configured `verify_command` runs first when the round never triggered
/// it: a completion claim made in a read-only round is usually claiming credit
/// for edits that landed earlier, and those still have to hold up.
///
/// Test-only: every surface can be interrupted while a check runs and uses
/// [`run_with_interrupt`].
#[cfg(test)]
pub async fn run(
    goal: &Goal,
    request: &VerifyRequest,
    sandbox: &Sandbox,
    cfg: &crate::config::Config,
) -> Option<CheckOutcome> {
    run_with_interrupt(goal, request, sandbox, cfg, std::future::pending())
        .await
        .0
}

/// Run the checks tier, abandoning it if `interrupt` resolves first.
///
/// A check is ordinary workspace work and can run for as long as a test suite
/// does, so an operator who interrupts must not wait for it. The command is
/// cancelled rather than merely dropped: cancellation terminates its process
/// group and reaps it, which dropping the future alone does not do.
pub async fn run_with_interrupt<F>(
    goal: &Goal,
    request: &VerifyRequest,
    sandbox: &Sandbox,
    cfg: &crate::config::Config,
    interrupt: F,
) -> (Option<CheckOutcome>, Interrupted)
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    let mut commands: Vec<(&str, VerificationKind)> = Vec::new();
    if request.run_verify_command
        && let Some(command) = cfg.verify_command.as_deref()
        && !command.trim().is_empty()
    {
        commands.push((command.trim(), VerificationKind::VerifyCommand));
    }
    if request.run_checks {
        commands.extend(
            goal.checks
                .iter()
                .map(|check| (check.command.as_str(), VerificationKind::Checks)),
        );
    }
    if commands.is_empty() {
        return (None, false);
    }

    let limits = limits(cfg);
    let mut verified = Vec::new();
    tokio::pin!(interrupt);
    let mut interrupt_pending = true;
    for (command, kind) in commands {
        if let Err(error) = crate::spend::run::admit_current_effect().await {
            return (
                Some(CheckOutcome {
                    failure_kind: CheckFailureKind::Unavailable,
                    all_passed: false,
                    failure_tail: Some(error.to_string()),
                    verified,
                }),
                false,
            );
        }
        let operation = crate::extras::validation::start_spooled(sandbox, command, limits);
        let cancellation = operation.cancellation();
        let wait = operation.wait();
        tokio::pin!(wait);
        let result = loop {
            tokio::select! {
                // Poll the signal first so a handler is installed before the
                // command can launch. The guard fuses the interrupt future:
                // once it has resolved it is never polled again.
                biased;
                signal = &mut interrupt, if interrupt_pending => {
                    interrupt_pending = false;
                    if signal.is_err() {
                        // A handler that could not be installed is not an
                        // interrupt (as in `unless_interrupted`): let the
                        // check run to completion.
                        tracing::warn!("goal check interrupt handler failed");
                        continue;
                    }
                    cancellation.cancel();
                    // The scoped worker reports only once the group is
                    // terminated and its direct child reaped.
                    let result = wait.await;
                    return (
                        Some(interrupted_outcome(command, &result, verified)),
                        true,
                    );
                }
                result = &mut wait => break result,
            }
        };
        if !result.succeeded() {
            // Timeout, cancellation, an output-limit breach, a launch failure,
            // and a non-zero exit are all failures. None of them is evidence
            // that the objective was reached.
            tracing::info!(
                goal = %goal.id,
                command = %crate::extras::validation::display_command(command),
                "goal check failed; the completion claim is rejected"
            );
            return (
                Some(CheckOutcome {
                    failure_kind: failure_kind(&result),
                    all_passed: false,
                    failure_tail: Some(format!(
                        "{} failed:\n{}",
                        crate::extras::validation::display_command(command),
                        result.render_tail(FAILURE_TAIL_CHARS)
                    )),
                    verified: Vec::new(),
                }),
                false,
            );
        }
        if !verified.contains(&kind) {
            verified.push(kind);
        }
    }

    (
        Some(CheckOutcome {
            failure_kind: CheckFailureKind::Exit,
            all_passed: true,
            failure_tail: None,
            verified,
        }),
        false,
    )
}

/// Whether a failed check is a finding about the work or about the check: a
/// timeout or a command that never started says nothing about the objective
/// and no edit fixes it.
fn failure_kind(result: &crate::extras::validation::ValidationResult) -> CheckFailureKind {
    use crate::extras::validation::ValidationStatus;
    match result.status {
        ValidationStatus::TimedOut | ValidationStatus::Failed => CheckFailureKind::Unavailable,
        _ => CheckFailureKind::Exit,
    }
}

/// A check cut short by an interrupt: never evidence the objective was met,
/// even when the cancelled command still exited zero.
fn interrupted_outcome(
    command: &str,
    result: &crate::extras::validation::ValidationResult,
    verified: Vec<VerificationKind>,
) -> CheckOutcome {
    CheckOutcome {
        failure_kind: CheckFailureKind::Exit,
        all_passed: false,
        failure_tail: Some(format!(
            "{} interrupted:\n{}",
            crate::extras::validation::display_command(command),
            result.render_tail(FAILURE_TAIL_CHARS)
        )),
        verified,
    }
}

/// Whether the operator interrupted the tier while it was running.
pub type Interrupted = bool;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::goal::GoalCheck;
    use crate::extras::goal::gate::VerifyCause;

    /// A real, unsandboxed shell so the checks actually execute; the loop
    /// validation tests bind one the same way.
    fn sandbox() -> Sandbox {
        use clap::Parser;
        let cli = crate::cli::Cli::parse_from([
            "mini-agent",
            "--no-session",
            "--no-sandbox",
            "--shell",
            "/bin/sh",
        ]);
        let cfg = crate::config::Config::default();
        let workspace = std::sync::Arc::new(
            crate::paths::WorkspaceBinding::capture(std::env::temp_dir().as_path()).unwrap(),
        );
        let authority = crate::permission::resolve_configured_execution_authority(&cli, &cfg)
            .unwrap()
            .0;
        crate::permission::bind_configured_shell(
            &cli,
            &cfg,
            authority,
            &workspace,
            None,
            Sandbox::new(false, "__missing_goal_check_backend__"),
        )
        .with_workspace_binding(workspace)
    }

    fn goal_with(checks: &[&str]) -> Goal {
        let mut goal = Goal::new("ship it", Vec::new()).unwrap();
        goal.checks = checks.iter().map(|c| GoalCheck::new(*c)).collect();
        goal
    }

    fn request(verify: bool, checks: bool) -> VerifyRequest {
        VerifyRequest {
            run_verify_command: verify,
            run_checks: checks,
            run_judge: false,
            cause: VerifyCause::MetClaim,
        }
    }

    #[tokio::test]
    async fn a_passing_check_reports_what_it_proved() {
        let goal = goal_with(&["true"]);
        let outcome = run(
            &goal,
            &request(false, true),
            &sandbox(),
            &Default::default(),
        )
        .await
        .expect("checks ran");
        assert!(
            outcome.all_passed,
            "`true` must pass: {:?}",
            outcome.failure_tail
        );
        assert_eq!(outcome.verified, vec![VerificationKind::Checks]);
        assert!(outcome.failure_tail.is_none());
    }

    /// A check that exits non-zero is a finding about the work; one that times
    /// out says nothing about it and is classed apart (mini-agent-sskdu.219).
    #[tokio::test]
    async fn a_timeout_is_unavailable_and_a_non_zero_exit_is_not() {
        use crate::extras::goal::gate::CheckFailureKind;
        let failing = run(
            &goal_with(&["false"]),
            &request(false, true),
            &sandbox(),
            &Default::default(),
        )
        .await
        .expect("checks ran");
        assert_eq!(failing.failure_kind, CheckFailureKind::Exit);

        let cfg = crate::config::Config {
            verify_timeout_secs: Some(1),
            ..Default::default()
        };
        let slow = run(
            &goal_with(&["sleep 30"]),
            &request(false, true),
            &sandbox(),
            &cfg,
        )
        .await
        .expect("checks ran");
        assert!(!slow.all_passed);
        assert_eq!(slow.failure_kind, CheckFailureKind::Unavailable);
    }

    #[tokio::test]
    async fn a_failing_check_rejects_the_claim_and_shows_the_command() {
        let goal = goal_with(&["true", "false"]);
        let outcome = run(
            &goal,
            &request(false, true),
            &sandbox(),
            &Default::default(),
        )
        .await
        .expect("checks ran");
        assert!(!outcome.all_passed);
        let tail = outcome.failure_tail.expect("failure is explained");
        assert!(
            tail.contains("false"),
            "the failing command is named: {tail}"
        );
        assert!(
            outcome.verified.is_empty(),
            "a failed run proves nothing, not even the checks that passed first"
        );
    }

    #[tokio::test]
    async fn a_check_that_times_out_is_a_failure_not_a_pass() {
        let goal = goal_with(&["sleep 30"]);
        let cfg = crate::config::Config {
            verify_timeout_secs: Some(1),
            ..Default::default()
        };
        let outcome = run(&goal, &request(false, true), &sandbox(), &cfg)
            .await
            .expect("checks ran");
        assert!(!outcome.all_passed);
        assert!(outcome.failure_tail.is_some());
    }

    /// Everything that is not "exited zero" is a failure.
    ///
    /// The specification lists them together for a reason: a launch that never
    /// happened and an output the harness had to truncate are both silence
    /// about whether the objective was reached, and silence is not proof.
    #[tokio::test]
    async fn a_check_that_cannot_speak_for_itself_is_a_failure() {
        // A command that does not exist never runs.
        let goal = goal_with(&["definitely-not-a-real-command-3f9a"]);
        let outcome = run(
            &goal,
            &request(false, true),
            &sandbox(),
            &Default::default(),
        )
        .await
        .expect("checks ran");
        assert!(!outcome.all_passed, "a launch failure proves nothing");
        assert!(outcome.verified.is_empty());

        // A command that floods its output is cut off, and a cut-off answer is
        // not an answer.
        let goal = goal_with(&["yes flooding-output"]);
        let outcome = run(
            &goal,
            &request(false, true),
            &sandbox(),
            &Default::default(),
        )
        .await
        .expect("checks ran");
        assert!(
            !outcome.all_passed,
            "an output-limit breach proves nothing either"
        );
        let tail = outcome.failure_tail.expect("failure is explained");
        // A megabyte of output reaches the model as a few thousand characters:
        // the tail plus the command it came from, not the flood itself.
        assert!(
            tail.chars().count() < FAILURE_TAIL_CHARS * 2,
            "and what the model is shown of it is bounded: {} chars",
            tail.chars().count()
        );
    }

    #[tokio::test]
    async fn the_verify_command_runs_before_the_goals_own_checks() {
        let goal = goal_with(&["true"]);
        let cfg = crate::config::Config {
            verify_command: Some("false".into()),
            ..Default::default()
        };
        let outcome = run(&goal, &request(true, true), &sandbox(), &cfg)
            .await
            .expect("checks ran");
        assert!(
            !outcome.all_passed,
            "a failing verify command rejects the claim before the checks run"
        );
        let tail = outcome.failure_tail.expect("failure is explained");
        assert!(tail.contains("false"));
    }

    #[tokio::test]
    async fn both_kinds_are_recorded_when_both_pass() {
        let goal = goal_with(&["true"]);
        let cfg = crate::config::Config {
            verify_command: Some("true".into()),
            ..Default::default()
        };
        let outcome = run(&goal, &request(true, true), &sandbox(), &cfg)
            .await
            .expect("checks ran");
        assert!(outcome.all_passed);
        assert!(outcome.verified.contains(&VerificationKind::VerifyCommand));
        assert!(outcome.verified.contains(&VerificationKind::Checks));
    }

    #[tokio::test]
    async fn nothing_to_run_means_no_outcome_rather_than_a_vacuous_pass() {
        let goal = goal_with(&[]);
        assert!(
            run(
                &goal,
                &request(false, true),
                &sandbox(),
                &Default::default()
            )
            .await
            .is_none(),
            "an empty check list must not be reported as verified"
        );
        assert!(
            run(
                &goal,
                &request(true, false),
                &sandbox(),
                &Default::default()
            )
            .await
            .is_none(),
            "a verify command that is not configured must not be reported as verified"
        );
    }

    /// A failing interrupt handler is not an interrupt: the check runs to
    /// completion (mini-agent-eonky).
    #[tokio::test]
    async fn a_failed_interrupt_handler_does_not_cancel_the_check() {
        let goal = goal_with(&["true", "true"]);
        let (outcome, interrupted) = run_with_interrupt(
            &goal,
            &request(false, true),
            &sandbox(),
            &Default::default(),
            async { Err(std::io::Error::other("no signal handler")) },
        )
        .await;
        assert!(!interrupted);
        assert!(outcome.expect("checks ran").all_passed);
    }

    /// An interrupt ends the tier at once, even when the cancelled command
    /// still exited zero, and the resolved interrupt is never polled again
    /// (mini-agent-kmmni).
    #[tokio::test]
    async fn an_interrupt_stops_the_tier_and_is_polled_once() {
        let goal = goal_with(&["true", "true"]);
        let (outcome, interrupted) = run_with_interrupt(
            &goal,
            &request(false, true),
            &sandbox(),
            &Default::default(),
            async { Ok(()) },
        )
        .await;
        assert!(interrupted);
        assert!(!outcome.expect("outcome").all_passed);
    }
}
