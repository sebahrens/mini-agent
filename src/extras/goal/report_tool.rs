//! The `goal_report` tool: the model's one channel into the goal record.
//!
//! The model can say what happened. It cannot change what it is being asked to
//! do, what counts as done, what verifies it, or how long it has. Those live on
//! the [`Goal`](super::Goal) and nothing here can reach them, so the objective
//! is read-only to the agent by construction rather than by instruction.
//!
//! A report is evidence for the gate, never a decision. Reporting `met` starts
//! verification; it does not finish a goal.
//!
//! Owning specification: `docs/superpowers/specs/2026-09-11-goal-feature-design.md`
//! §4.3.

use compact_str::CompactString;
use rig::tool::Tool;
use serde::Deserialize;

use super::{GoalStore, Report, ReportStatus};
use crate::agent::tools::ToolError;

/// Caps on the free text a report may carry. These bound the preamble the gate
/// feeds back and the transcript tail the judge sees.
pub const MAX_EVIDENCE_CHARS: usize = 2_000;
pub const MAX_BLOCKER_CHARS: usize = 1_000;
pub const MAX_REASON_CHARS: usize = 1_000;
pub const MAX_QUESTION_CHARS: usize = 1_000;

#[derive(Debug, Deserialize)]
pub struct GoalReportArgs {
    pub status: String,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub blocker: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub question: Option<String>,
}

fn parse_status(raw: &str) -> Result<ReportStatus, ToolError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "progress" => Ok(ReportStatus::Progress),
        "met" => Ok(ReportStatus::Met),
        "blocked" => Ok(ReportStatus::Blocked),
        "impossible" => Ok(ReportStatus::Impossible),
        "needs_user" | "needs-user" => Ok(ReportStatus::NeedsUser),
        other => Err(ToolError::Msg(format!(
            "unknown status {other:?}; expected progress, met, blocked, impossible, or needs_user"
        ))),
    }
}

fn check_len(value: &str, field: &str, max: usize) -> Result<(), ToolError> {
    let len = value.chars().count();
    if len > max {
        return Err(ToolError::Msg(format!(
            "{field} is {len} characters; the maximum is {max}"
        )));
    }
    Ok(())
}

/// Validate one report against the rules for its status.
fn build_report(args: GoalReportArgs, round: u32) -> Result<Report, ToolError> {
    let status = parse_status(&args.status)?;

    let trimmed = |value: Option<String>| -> Option<String> {
        value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let evidence = trimmed(args.evidence);
    let blocker = trimmed(args.blocker);
    let reason = trimmed(args.reason);
    let question = trimmed(args.question);

    if let Some(v) = &evidence {
        check_len(v, "evidence", MAX_EVIDENCE_CHARS)?;
    }
    if let Some(v) = &blocker {
        check_len(v, "blocker", MAX_BLOCKER_CHARS)?;
    }
    if let Some(v) = &reason {
        check_len(v, "reason", MAX_REASON_CHARS)?;
    }
    if let Some(v) = &question {
        check_len(v, "question", MAX_QUESTION_CHARS)?;
    }

    // Each status carries the one field that makes it actionable. Without it
    // the gate would have nothing to show the user or feed back to the model.
    match status {
        ReportStatus::Met if evidence.is_none() => {
            return Err(ToolError::Msg(
                "status \"met\" requires evidence: state what you verified and how".into(),
            ));
        }
        ReportStatus::Blocked if blocker.is_none() => {
            return Err(ToolError::Msg(
                "status \"blocked\" requires a blocker describing what is in the way".into(),
            ));
        }
        ReportStatus::Impossible if reason.is_none() => {
            return Err(ToolError::Msg(
                "status \"impossible\" requires a reason the objective cannot be satisfied".into(),
            ));
        }
        ReportStatus::NeedsUser if question.is_none() => {
            return Err(ToolError::Msg(
                "status \"needs_user\" requires the question to put to the user".into(),
            ));
        }
        _ => {}
    }

    Ok(Report {
        status,
        evidence,
        blocker,
        reason,
        question,
        round,
        at: CompactString::new(chrono::Utc::now().to_rfc3339()),
    })
}

/// The `goal_report` tool.
pub struct GoalReport {
    store: GoalStore,
}

impl GoalReport {
    pub fn new(store: GoalStore) -> Self {
        Self { store }
    }
}

impl Tool for GoalReport {
    const NAME: &'static str = "goal_report";

    type Error = ToolError;
    type Args = GoalReportArgs;
    type Output = String;

    fn description(&self) -> String {
        "Report progress toward the active goal. Call this before ending a turn. \
         Use \"met\" only with evidence the conversation can demonstrate, \"blocked\" \
         only for something you cannot remove yourself, \"impossible\" only if the \
         objective cannot be satisfied as written, and \"needs_user\" when you need \
         an answer to continue. Reporting \"met\" starts verification; it does not \
         end the goal."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["progress", "met", "blocked", "impossible", "needs_user"],
                    "description": "What happened this turn."
                },
                "evidence": {
                    "type": "string",
                    "description": "Required for \"met\": what you verified and how."
                },
                "blocker": {
                    "type": "string",
                    "description": "Required for \"blocked\": what is in the way."
                },
                "reason": {
                    "type": "string",
                    "description": "Required for \"impossible\": why the objective cannot be satisfied."
                },
                "question": {
                    "type": "string",
                    "description": "Required for \"needs_user\": the question to put to the user."
                }
            },
            "required": ["status"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: GoalReportArgs) -> Result<String, ToolError> {
        let Some(goal) = self.store.snapshot() else {
            return Err(ToolError::Msg(
                "no goal is active; goal_report has nothing to report against".into(),
            ));
        };
        if goal.status.is_terminal() {
            return Err(ToolError::Msg(format!(
                "the goal is already {}; goal_report is not accepted",
                goal.status.label()
            )));
        }

        // Reports belong to the round that is about to be judged.
        let round = goal.progress.rounds + 1;
        let report = build_report(args, round)?;
        let status = report.status;
        self.store.append_report(report);

        tracing::debug!(
            round,
            ?status,
            goal = %goal.id,
            "tool goal_report recorded"
        );

        Ok(match status {
            ReportStatus::Met => format!(
                "Completion claim recorded for round {round}. It will be verified before the goal is closed."
            ),
            ReportStatus::NeedsUser => format!(
                "Question recorded for round {round}. The goal pauses for the user's answer."
            ),
            _ => format!(
                "Report recorded for round {round} of {}.",
                goal.bounds.max_rounds
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::goal::{Goal, GoalStatus};

    fn store_with_goal() -> GoalStore {
        let store = GoalStore::default();
        store
            .set(Goal::new("ship it", Vec::new()).unwrap(), false)
            .unwrap();
        store
    }

    fn args(status: &str) -> GoalReportArgs {
        GoalReportArgs {
            status: status.into(),
            evidence: None,
            blocker: None,
            reason: None,
            question: None,
        }
    }

    #[tokio::test]
    async fn a_progress_report_is_recorded_against_the_coming_round() {
        let store = store_with_goal();
        store.with_mut(|g| g.progress.rounds = 4);
        let tool = GoalReport::new(store.clone());

        let out = tool.call(args("progress")).await.expect("accepted");
        assert!(out.contains("round 5"));

        let goal = store.snapshot().unwrap();
        let report = goal.last_report().expect("recorded");
        assert_eq!(report.status, ReportStatus::Progress);
        assert_eq!(report.round, 5);
    }

    #[tokio::test]
    async fn each_status_requires_the_field_that_makes_it_actionable() {
        let tool = GoalReport::new(store_with_goal());

        for (status, field) in [
            ("met", "evidence"),
            ("blocked", "blocker"),
            ("impossible", "reason"),
            ("needs_user", "question"),
        ] {
            let err = tool.call(args(status)).await.unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains(field),
                "status {status} should demand {field}, said: {message}"
            );
        }

        // Progress needs nothing beyond the status.
        assert!(tool.call(args("progress")).await.is_ok());
    }

    #[tokio::test]
    async fn a_complete_report_of_each_status_is_accepted() {
        let store = store_with_goal();
        let tool = GoalReport::new(store.clone());

        let cases = [
            (
                "met",
                GoalReportArgs {
                    status: "met".into(),
                    evidence: Some("cargo test passed".into()),
                    blocker: None,
                    reason: None,
                    question: None,
                },
            ),
            (
                "blocked",
                GoalReportArgs {
                    status: "blocked".into(),
                    evidence: None,
                    blocker: Some("no network".into()),
                    reason: None,
                    question: None,
                },
            ),
            (
                "impossible",
                GoalReportArgs {
                    status: "impossible".into(),
                    evidence: None,
                    blocker: None,
                    reason: Some("the API was withdrawn".into()),
                    question: None,
                },
            ),
            (
                "needs_user",
                GoalReportArgs {
                    status: "needs_user".into(),
                    evidence: None,
                    blocker: None,
                    reason: None,
                    question: Some("which database?".into()),
                },
            ),
        ];
        for (name, a) in cases {
            tool.call(a).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert_eq!(store.snapshot().unwrap().reports.len(), 4);
    }

    #[tokio::test]
    async fn an_unknown_status_is_rejected_by_name() {
        let tool = GoalReport::new(store_with_goal());
        let err = tool.call(args("done")).await.unwrap_err();
        assert!(err.to_string().contains("unknown status"));
        assert!(err.to_string().contains("needs_user"));
    }

    #[tokio::test]
    async fn oversized_free_text_is_rejected() {
        let tool = GoalReport::new(store_with_goal());
        let err = tool
            .call(GoalReportArgs {
                status: "met".into(),
                evidence: Some("x".repeat(MAX_EVIDENCE_CHARS + 1)),
                blocker: None,
                reason: None,
                question: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("evidence is"));
        assert!(err.to_string().contains(&MAX_EVIDENCE_CHARS.to_string()));
    }

    #[tokio::test]
    async fn reporting_without_a_goal_or_against_a_finished_one_is_refused() {
        let empty = GoalReport::new(GoalStore::default());
        assert!(
            empty
                .call(args("progress"))
                .await
                .unwrap_err()
                .to_string()
                .contains("no goal is active")
        );

        let store = store_with_goal();
        store.with_mut(|g| g.set_status(GoalStatus::Met, None));
        let done = GoalReport::new(store);
        assert!(
            done.call(args("progress"))
                .await
                .unwrap_err()
                .to_string()
                .contains("already met")
        );
    }

    /// The schema is the whole guarantee that the model cannot edit what it is
    /// being judged against, so it is asserted rather than assumed.
    #[test]
    fn the_schema_exposes_no_way_to_change_the_objective_or_its_bounds() {
        let tool = GoalReport::new(GoalStore::default());
        let schema = tool.parameters();
        let properties = schema["properties"].as_object().expect("object schema");

        let mut names: Vec<_> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["blocker", "evidence", "question", "reason", "status"],
            "goal_report must expose exactly the reporting fields"
        );
        assert_eq!(
            schema["additionalProperties"],
            serde_json::Value::Bool(false),
            "unknown fields must be rejected outright"
        );
        assert_eq!(GoalReport::NAME, "goal_report");
    }

    #[tokio::test]
    async fn a_report_never_changes_the_goal_itself() {
        let store = store_with_goal();
        let before = store.snapshot().unwrap();
        let tool = GoalReport::new(store.clone());
        tool.call(GoalReportArgs {
            status: "met".into(),
            evidence: Some("all done".into()),
            blocker: None,
            reason: None,
            question: None,
        })
        .await
        .unwrap();

        let after = store.snapshot().unwrap();
        assert_eq!(after.objective, before.objective);
        assert_eq!(after.criteria, before.criteria);
        assert_eq!(after.checks, before.checks);
        assert_eq!(after.bounds, before.bounds);
        assert_eq!(after.judge, before.judge);
        assert_eq!(
            after.status,
            GoalStatus::Active,
            "claiming completion does not complete the goal"
        );
        assert_eq!(after.last_verdict, before.last_verdict);
    }

    #[tokio::test]
    async fn whitespace_only_fields_count_as_missing() {
        let tool = GoalReport::new(store_with_goal());
        let err = tool
            .call(GoalReportArgs {
                status: "blocked".into(),
                evidence: None,
                blocker: Some("   \n ".into()),
                reason: None,
                question: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires a blocker"));
    }
}
