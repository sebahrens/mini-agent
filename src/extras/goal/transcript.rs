//! One durable record per gate evaluation.
//!
//! A goal decides things a user did not watch happen: it kept going, it
//! refused a completion claim, it stopped on a budget. The record is how those
//! decisions stay auditable afterwards, so it holds what was checked and what
//! was concluded rather than the conversation that produced it.
//!
//! Fields are bounded and the whole writer is suppressed when artifact writing
//! is disabled. Ownership is declared in `docs/specs/platform-paths.md`.
//!
//! Owning specification: `docs/specs/goals.md` (Surfaces).

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::Goal;
use super::gate::{CheckOutcome, GateDecision, JudgeOutcome, RoundSummary};

/// Cap on any single free-text field in a record.
const FIELD_CHARS: usize = 4_000;

#[derive(Serialize)]
struct RoundRecord<'a> {
    round: u32,
    timestamp: String,
    goal_id: &'a str,
    /// Digest rather than text: the instruction restates the objective, which
    /// is already recorded, and a digest still proves which one ran.
    instruction_sha256: String,
    status_after: &'a str,
    decision: &'a str,
    reason: String,
    tool_calls: u32,
    mutating_tool_calls: u32,
    report: Option<&'a super::Report>,
    checks: Option<CheckRecord>,
    judge: Option<JudgeRecord>,
    evidence: Vec<&'a str>,
}

#[derive(Serialize)]
struct CheckRecord {
    all_passed: bool,
    failure_tail: Option<String>,
}

#[derive(Serialize)]
struct JudgeRecord {
    outcome: &'static str,
    reason: String,
    /// Which model answered, and whether it was the agent's own. A second
    /// opinion from the model that just did the work is weaker than one from a
    /// distinct model, so the record says which it was rather than leaving a
    /// reader to assume the stronger reading.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    same_model_as_agent: bool,
}

fn clip(text: &str) -> String {
    if text.chars().count() <= FIELD_CHARS {
        return text.to_string();
    }
    text.chars().take(FIELD_CHARS).collect::<String>() + "…"
}

/// Write the record for one gate evaluation.
///
/// A failure here is reported and dropped: losing an audit line must never turn
/// a decided round into a failed one.
pub fn save_round(
    goal: &Goal,
    summary: &RoundSummary,
    decision: &GateDecision,
    instruction: &str,
    checks: Option<&CheckOutcome>,
    judge: Option<&JudgeOutcome>,
) {
    if crate::paths::artifact_disabled("goal transcripts") {
        return;
    }
    let Ok(paths) = crate::paths::process_paths() else {
        return;
    };

    let record = build_record(goal, summary, decision, instruction, checks, judge);

    let dir = paths.goals_dir().join(goal.id.as_str());
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(%error, "goal: could not create the transcript directory");
        return;
    }
    let path = dir.join(format!("round-{:04}.json", goal.progress.rounds));
    match serde_json::to_vec_pretty(&record) {
        Ok(bytes) => {
            if let Err(error) = crate::fs::private_atomic_write_sync(&path, &bytes) {
                tracing::warn!(%error, "goal: could not write the round transcript");
            }
        }
        Err(error) => tracing::warn!(%error, "goal: could not serialize the round transcript"),
    }
}

/// Assemble the record for one gate evaluation.
///
/// Split from the write so the shape can be asserted without a filesystem, and
/// so the tests that do assert it are looking at the very record that gets
/// written rather than a second one built to match.
fn build_record<'a>(
    goal: &'a Goal,
    summary: &'a RoundSummary,
    decision: &GateDecision,
    instruction: &str,
    checks: Option<&CheckOutcome>,
    judge: Option<&JudgeOutcome>,
) -> RoundRecord<'a> {
    RoundRecord {
        round: goal.progress.rounds,
        timestamp: chrono::Utc::now().to_rfc3339(),
        goal_id: goal.id.as_str(),
        instruction_sha256: crate::hex::encode_lower(Sha256::digest(instruction.as_bytes())),
        status_after: goal.status.label(),
        decision: match decision {
            GateDecision::Continue { wrap_up: true, .. } => "wrap_up",
            GateDecision::Continue { .. } => "continue",
            GateDecision::Stop { .. } => "stop",
        },
        reason: clip(decision.reason()),
        tool_calls: summary.tool_calls,
        mutating_tool_calls: summary.mutating_tool_calls,
        report: summary.report.as_ref(),
        checks: checks.map(|outcome| CheckRecord {
            all_passed: outcome.all_passed,
            failure_tail: outcome.failure_tail.as_deref().map(clip),
        }),
        judge: judge.map(|outcome| {
            let model = goal
                .resolved_judge
                .as_ref()
                .map(|resolved| resolved.describe());
            let same_model_as_agent = goal
                .resolved_judge
                .as_ref()
                .is_some_and(|resolved| resolved.same_as_session);
            match outcome {
                JudgeOutcome::Verdict { outcome, reason } => JudgeRecord {
                    outcome: match outcome {
                        super::Outcome::Met => "met",
                        super::Outcome::NotYet => "not_yet",
                        super::Outcome::Impossible => "impossible",
                    },
                    reason: clip(reason),
                    model,
                    same_model_as_agent,
                },
                JudgeOutcome::Unavailable { reason } => JudgeRecord {
                    outcome: "unavailable",
                    reason: clip(reason),
                    model,
                    same_model_as_agent,
                },
            }
        }),
        evidence: goal
            .last_verdict
            .as_ref()
            .map(super::Verdict::evidence_labels)
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::goal::VerdictSource;
    use crate::extras::goal::gate::RoundEnd;
    use crate::extras::goal::{GoalStatus, Outcome, Report, ReportStatus, VerificationKind};

    fn goal() -> Goal {
        let mut goal = Goal::new("ship it", Vec::new()).unwrap();
        goal.progress.rounds = 3;
        goal.last_verdict = Some(crate::extras::goal::Verdict {
            outcome: Outcome::Met,
            reason: "cargo test passed".into(),
            source: VerdictSource::Checks,
            evidence: vec![VerificationKind::SelfReport, VerificationKind::Checks],
            at: compact_str::CompactString::new("now"),
        });
        goal.set_status(GoalStatus::Met, None);
        goal
    }

    fn summary() -> RoundSummary {
        RoundSummary {
            end: RoundEnd::Done,
            tool_calls: 5,
            mutating_tool_calls: 2,
            report: Some(Report {
                status: ReportStatus::Met,
                evidence: Some("did the work".into()),
                blocker: None,
                reason: None,
                question: None,
                round: 3,
                at: compact_str::CompactString::new("now"),
            }),
            verify_ran: true,
            verify_passed: Some(true),
            verify_configured: true,
            open_todos: 0,
            tokens_used: 100,
            active: std::time::Duration::from_secs(5),
        }
    }

    /// Render the very record `save_round` writes, without touching the
    /// filesystem. The write itself is a `create_dir_all` plus an atomic write
    /// and is exercised by the real-binary scenario.
    fn render(goal: &Goal, checks: Option<&CheckOutcome>, judge: Option<&JudgeOutcome>) -> String {
        let summary = summary();
        let decision = GateDecision::Stop {
            status: GoalStatus::Met,
            reason: "cargo test passed".into(),
            paused_reason: None,
            source: VerdictSource::Checks,
            evidence: vec![VerificationKind::SelfReport, VerificationKind::Checks],
        };
        let record = build_record(goal, &summary, &decision, "instruction", checks, judge);
        serde_json::to_string_pretty(&record).unwrap()
    }

    #[test]
    fn a_record_says_what_was_decided_and_what_proved_it() {
        let json = render(
            &goal(),
            Some(&CheckOutcome {
                all_passed: true,
                failure_tail: None,
                verified: vec![VerificationKind::Checks],
            }),
            Some(&JudgeOutcome::Verdict {
                outcome: Outcome::Met,
                reason: "confirmed".into(),
            }),
        );
        assert!(json.contains("\"status_after\": \"met\""));
        assert!(json.contains("\"decision\": \"stop\""));
        assert!(json.contains("cargo test passed"));
        assert!(json.contains("\"all_passed\": true"));
        assert!(json.contains("\"outcome\": \"met\""));
        assert!(json.contains("checks"));
        assert!(json.contains("\"mutating_tool_calls\": 2"));
    }

    /// The instruction restates an objective already recorded elsewhere, so the
    /// record keeps a digest that proves which one ran without copying it.
    #[test]
    fn the_instruction_is_recorded_as_a_digest_not_as_text() {
        let json = render(&goal(), None, None);
        assert!(!json.contains("instruction\":"));
        assert!(json.contains("instruction_sha256"));
        assert!(
            json.contains(&crate::hex::encode_lower(Sha256::digest(b"instruction"))),
            "the digest identifies the instruction"
        );
    }

    #[test]
    fn an_unavailable_judge_is_recorded_as_such() {
        let json = render(
            &goal(),
            None,
            Some(&JudgeOutcome::Unavailable {
                reason: "connection refused".into(),
            }),
        );
        assert!(json.contains("\"outcome\": \"unavailable\""));
        assert!(json.contains("connection refused"));
    }

    #[test]
    fn free_text_fields_are_bounded() {
        let long = "x".repeat(FIELD_CHARS * 2);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), FIELD_CHARS + 1);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip("short"), "short");
    }
}
