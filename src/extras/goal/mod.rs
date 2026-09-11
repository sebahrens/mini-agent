//! Persistent, verifiable goals.
//!
//! A goal is a durable objective the agent keeps working toward across turns.
//! It is held outside the conversation so that compaction, agent rebuilds, and
//! session reloads cannot lose it, and it is re-read from this record — never
//! from message history — every time the objective is shown to the model.
//!
//! A goal advances in **rounds**: one agent run followed by one gate
//! evaluation. The gate ([`gate`]) is a pure function; the driver owns every
//! side effect and is the only writer of [`GoalStatus`] and [`GoalProgress`].
//! The model's sole influence on this record is appending a [`Report`] through
//! the `goal_report` tool, so the objective, criteria, checks, judge, and
//! bounds are read-only to it by construction.
//!
//! Owning specification: `docs/superpowers/specs/2026-09-11-goal-feature-design.md`
//! §4.1 (record and shared store).

// The record lands before the code that reads it: the preamble block, the
// `goal_report` tool, the gate, and the driver arrive in mini-agent-a1qwa.2
// through .6. Until the `/goal` surface exists every accessor here is
// unreachable from the binary, which the strict lint rows would reject. This
// allow is removed by mini-agent-a1qwa.6, at which point each item has a
// production caller.
#![allow(dead_code)]

pub mod driver;
pub mod gate;
pub mod prompt;
pub mod report_tool;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use compact_str::CompactString;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum objective length. Matches the cap the surveyed harnesses use and
/// keeps the static preamble block bounded.
pub const MAX_OBJECTIVE_CHARS: usize = 4_000;
/// Maximum number of "done when" criteria.
pub const MAX_CRITERIA: usize = 16;
/// Maximum length of a single criterion.
pub const MAX_CRITERION_CHARS: usize = 500;
/// Retained [`Report`] history. Bounded so a long goal cannot grow the session
/// file without limit.
pub const MAX_REPORTS: usize = 8;

/// Why a goal is parked in [`GoalStatus::Paused`].
///
/// `Paused` is reachable from four unrelated runtime conditions; without this
/// discriminator a user cannot tell "your judge is down" from "the model
/// stalled".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// Rounds elapsed with neither a mutating tool call nor a new report.
    NoProgress,
    /// The judge failed repeatedly on completion claims.
    JudgeUnavailable,
    /// Compaction could not recover enough context to continue.
    ContextOverflow,
    /// Consecutive agent runs failed before the gate could judge them.
    RoundFailure,
    /// A persisted session carried a status this build does not know.
    UnknownStatusOnLoad,
}

impl PauseReason {
    /// Operator-facing text. Shown by `/goal status` and the headless summary.
    pub fn describe(self) -> &'static str {
        match self {
            Self::NoProgress => "no progress for several rounds",
            Self::JudgeUnavailable => "the completion judge was unavailable",
            Self::ContextOverflow => "the context window overflowed unrecoverably",
            Self::RoundFailure => "consecutive agent runs failed",
            Self::UnknownStatusOnLoad => "the stored goal status is not recognized",
        }
    }
}

/// Lifecycle state of a goal.
///
/// Only [`GoalStatus::Met`] and [`GoalStatus::Impossible`] are terminal. Every
/// other stop is resumable, so exhausting a budget or hitting a blocker parks
/// the goal for the user rather than discarding the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// Being worked on.
    Active,
    /// The agent asked the user a question and is waiting for the answer.
    AwaitingUser,
    /// Parked by the harness; see [`Goal::paused_reason`].
    Paused,
    /// The agent reported the same blocker for several rounds without progress.
    Blocked,
    /// A bound was exhausted after the wrap-up round.
    BudgetLimited,
    /// The objective was reached and verified.
    Met,
    /// The objective cannot be satisfied as written.
    Impossible,
}

impl GoalStatus {
    /// Whether the goal is finished and will not resume on its own.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Met | Self::Impossible)
    }

    /// Whether the driver should keep running rounds for this status.
    pub fn is_running(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Whether a transition is allowed. Terminal states leave only by an
    /// explicit user action (`/goal reopen` or `/goal clear`), so the driver
    /// can never resurrect a finished goal on its own.
    pub fn can_transition(self, to: Self) -> bool {
        if self == to {
            return true;
        }
        match self {
            // A finished goal only reopens by explicit user action, which is
            // modelled as Impossible -> Active. `Met` is final.
            Self::Met => false,
            Self::Impossible => to == Self::Active,
            // Every parked state resumes, and any live state may stop.
            Self::Active
            | Self::AwaitingUser
            | Self::Paused
            | Self::Blocked
            | Self::BudgetLimited => true,
        }
    }

    /// Short label for the status line and JSON output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::AwaitingUser => "awaiting user",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::BudgetLimited => "budget limited",
            Self::Met => "met",
            Self::Impossible => "impossible",
        }
    }
}

/// What a completion verdict was actually backed by.
///
/// Recorded on every [`Verdict`] so a `Met` reached only by the model's own
/// account is distinguishable from one a command proved, both in the UI and in
/// downstream evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationKind {
    /// The model said so.
    SelfReport,
    /// Configured goal checks exited zero.
    Checks,
    /// The configured `verify_command` exited zero.
    VerifyCommand,
    /// A judge model agreed.
    Judge,
}

impl VerificationKind {
    /// Whether this kind is external proof rather than the model's own claim.
    ///
    /// Only these kinds may carry a goal verdict into skill-promotion
    /// evidence; a judge reading a transcript the worker wrote is not proof.
    pub fn is_external_proof(self) -> bool {
        matches!(self, Self::Checks | Self::VerifyCommand)
    }
}

/// Outcome of one completion evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Not finished; keep working.
    NotYet,
    /// Finished and verified to the configured depth.
    Met,
    /// Cannot be finished as written.
    Impossible,
}

/// Which layer produced a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictSource {
    /// Todo state, mutation counters, or verification state.
    Structural,
    /// Goal checks or `verify_command`.
    Checks,
    /// A judge model.
    Judge,
    /// The model's own `goal_report` call.
    ModelReport,
    /// A configured bound was exhausted.
    Bounds,
    /// A harness condition such as a failed run or context overflow.
    Runtime,
}

/// One gate decision, retained for display and audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub outcome: Outcome,
    pub reason: String,
    pub source: VerdictSource,
    /// Everything that backed this verdict, lowest proof first.
    #[serde(default)]
    pub evidence: Vec<VerificationKind>,
    pub at: CompactString,
}

impl Verdict {
    /// Whether any external command proved this verdict.
    pub fn externally_verified(&self) -> bool {
        self.evidence.iter().any(|kind| kind.is_external_proof())
    }
}

/// What the model claimed at the end of a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// Work happened; keep going.
    Progress,
    /// The objective is complete.
    Met,
    /// Something outside the agent's control is in the way.
    Blocked,
    /// The objective cannot be satisfied as written.
    Impossible,
    /// The agent needs an answer from the user.
    NeedsUser,
}

/// One `goal_report` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub status: ReportStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    pub round: u32,
    pub at: CompactString,
}

/// A command that must exit zero before a completion claim is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCheck {
    pub command: String,
}

impl GoalCheck {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
        }
    }
}

/// Which model judges completion claims.
///
/// The judge is whatever model the user names: a different family or the same,
/// smaller, larger, or identical to the agent's own, on any provider. No
/// relationship to the agent model is assumed, and a single-model installation
/// still works because [`JudgePolicy::Auto`] falls back to the session model in
/// a fresh context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgePolicy {
    /// Resolve at goal creation: the configured judge model, else a
    /// `quick_models` entry named `goal_judge`, else the session model.
    #[default]
    Auto,
    /// A named `quick_models` entry.
    QuickModel(String),
    /// The session's own model in a fresh context.
    Session,
    /// No judge tier.
    Off,
}

/// The model [`JudgePolicy::Auto`] actually selected, recorded so the choice is
/// visible and auditable rather than implicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedJudge {
    /// Display label: a `quick_models` entry name, or `session`.
    pub label: CompactString,
    pub provider: CompactString,
    pub model: CompactString,
    /// True when the judge is the agent's own model, which the UI labels so a
    /// same-model verdict is never mistaken for an independent one.
    #[serde(default)]
    pub same_as_session: bool,
}

impl ResolvedJudge {
    /// Operator-facing description for `/goal status`.
    pub fn describe(&self) -> String {
        if self.same_as_session {
            format!(
                "session model ({}/{}, fresh context)",
                self.provider, self.model
            )
        } else {
            format!("{} ({}/{})", self.label, self.provider, self.model)
        }
    }
}

/// How a goal keeps working after a round ends without completion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationMode {
    /// Relaunch with the session history retained.
    #[default]
    Continue,
    /// Relaunch with empty history and a driver-built summary, the amnesiac
    /// iteration `--loop` performs today.
    Restart { summary_chars: usize },
}

/// Default summary budget carried between [`ContinuationMode::Restart`] rounds.
pub const DEFAULT_RESTART_SUMMARY_CHARS: usize = 1_024;

/// Limits enforced by the harness rather than by prompt text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalBounds {
    /// Maximum rounds (agent run plus gate evaluation) before wrapping up.
    pub max_rounds: u32,
    /// Optional cumulative input+output token budget across rounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Optional agent-active wall clock, excluding time blocked on the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_active_secs: Option<u64>,
    /// Rounds without progress before parking the goal.
    pub no_progress_rounds: u32,
    /// Rounds reporting a blocker without progress before stopping.
    pub blocked_rounds: u32,
    /// Provider calls allowed in the bounded wrap-up round.
    pub wrap_up_max_agent_turns: u32,
    /// Provider calls between in-round goal reminders.
    pub reinject_every: u32,
    /// Rounds between judge drift checks.
    pub judge_every: u32,
}

impl Default for GoalBounds {
    fn default() -> Self {
        Self {
            max_rounds: 50,
            max_tokens: None,
            max_active_secs: None,
            no_progress_rounds: 3,
            blocked_rounds: 3,
            wrap_up_max_agent_turns: 4,
            reinject_every: 6,
            judge_every: 5,
        }
    }
}

/// Counters the driver maintains across rounds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalProgress {
    pub rounds: u32,
    pub tokens_used: u64,
    pub active_secs: u64,
    pub consecutive_no_progress: u32,
    pub consecutive_blocked: u32,
    pub consecutive_round_failures: u32,
    /// Set when the bounded wrap-up round has been issued, so it is issued
    /// exactly once per exhaustion.
    pub wrap_up_issued: bool,
    /// Consecutive judge failures, for the fail-open ladder.
    pub judge_failures: u32,
}

/// Rejected goal construction or mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalError {
    EmptyObjective,
    ObjectiveTooLong {
        len: usize,
    },
    TooManyCriteria {
        len: usize,
    },
    CriterionTooLong {
        index: usize,
        len: usize,
    },
    /// A non-terminal goal already exists and replacement was not requested.
    AlreadyActive {
        status: GoalStatus,
    },
}

impl std::fmt::Display for GoalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyObjective => write!(f, "a goal needs a non-empty objective"),
            Self::ObjectiveTooLong { len } => write!(
                f,
                "objective is {len} characters; the maximum is {MAX_OBJECTIVE_CHARS}"
            ),
            Self::TooManyCriteria { len } => {
                write!(f, "{len} criteria given; the maximum is {MAX_CRITERIA}")
            }
            Self::CriterionTooLong { index, len } => write!(
                f,
                "criterion {} is {len} characters; the maximum is {MAX_CRITERION_CHARS}",
                index + 1
            ),
            Self::AlreadyActive { status } => write!(
                f,
                "a goal is already {}; use /goal clear first",
                status.label()
            ),
        }
    }
}

impl std::error::Error for GoalError {}

fn now_rfc3339() -> CompactString {
    CompactString::new(chrono::Utc::now().to_rfc3339())
}

/// A persistent objective and everything the harness needs to pursue it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub id: CompactString,
    /// User-provided task data. Never treated as instructions that outrank the
    /// system prompt; the preamble fences it accordingly.
    pub objective: String,
    /// Optional "done when" bullets. Read-only to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<String>,
    pub status: GoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<PauseReason>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<GoalCheck>,
    #[serde(default)]
    pub judge: JudgePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_judge: Option<ResolvedJudge>,
    #[serde(default)]
    pub continuation: ContinuationMode,
    #[serde(default)]
    pub bounds: GoalBounds,
    #[serde(default)]
    pub progress: GoalProgress,
    /// The most recent `goal_report` calls, newest last.
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    pub reports: VecDeque<Report>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verdict: Option<Verdict>,
    pub created_at: CompactString,
    pub updated_at: CompactString,
}

impl Goal {
    /// Build a goal, validating the caps that keep the preamble bounded.
    pub fn new(objective: impl Into<String>, criteria: Vec<String>) -> Result<Self, GoalError> {
        let objective = objective.into();
        let trimmed = objective.trim();
        if trimmed.is_empty() {
            return Err(GoalError::EmptyObjective);
        }
        let len = trimmed.chars().count();
        if len > MAX_OBJECTIVE_CHARS {
            return Err(GoalError::ObjectiveTooLong { len });
        }
        if criteria.len() > MAX_CRITERIA {
            return Err(GoalError::TooManyCriteria {
                len: criteria.len(),
            });
        }
        for (index, criterion) in criteria.iter().enumerate() {
            let len = criterion.chars().count();
            if len > MAX_CRITERION_CHARS {
                return Err(GoalError::CriterionTooLong { index, len });
            }
        }
        let now = now_rfc3339();
        Ok(Self {
            id: CompactString::new(uuid::Uuid::new_v4().to_string()),
            objective: trimmed.to_string(),
            criteria: criteria
                .into_iter()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect(),
            status: GoalStatus::Active,
            paused_reason: None,
            checks: Vec::new(),
            judge: JudgePolicy::default(),
            resolved_judge: None,
            continuation: ContinuationMode::default(),
            bounds: GoalBounds::default(),
            progress: GoalProgress::default(),
            reports: VecDeque::new(),
            last_verdict: None,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Move to a new status, recording the pause reason and refusing
    /// transitions out of a finished goal.
    pub fn set_status(&mut self, status: GoalStatus, paused_reason: Option<PauseReason>) -> bool {
        if !self.status.can_transition(status) {
            return false;
        }
        self.status = status;
        self.paused_reason = if status == GoalStatus::Paused {
            paused_reason
        } else {
            None
        };
        self.touch();
        true
    }

    /// Append a model report, discarding the oldest beyond [`MAX_REPORTS`].
    pub fn push_report(&mut self, report: Report) {
        self.reports.push_back(report);
        while self.reports.len() > MAX_REPORTS {
            self.reports.pop_front();
        }
        self.touch();
    }

    /// The newest report, if any.
    pub fn last_report(&self) -> Option<&Report> {
        self.reports.back()
    }

    /// Reports recorded during the given round.
    pub fn reports_in_round(&self, round: u32) -> impl Iterator<Item = &Report> {
        self.reports.iter().filter(move |r| r.round == round)
    }

    /// Clear the parked state so the goal runs again.
    pub fn resume(&mut self) -> bool {
        if self.status.is_terminal() {
            return false;
        }
        self.progress.wrap_up_issued = false;
        self.progress.consecutive_no_progress = 0;
        self.progress.consecutive_blocked = 0;
        self.progress.consecutive_round_failures = 0;
        self.set_status(GoalStatus::Active, None)
    }

    /// Reopen a goal that was declared impossible.
    pub fn reopen(&mut self) -> bool {
        if self.status != GoalStatus::Impossible {
            return false;
        }
        self.progress.wrap_up_issued = false;
        self.set_status(GoalStatus::Active, None)
    }

    pub fn touch(&mut self) {
        self.updated_at = now_rfc3339();
    }

    /// Whether the objective reached completion without any external command
    /// proving it. The UI labels such a goal rather than presenting it as
    /// verified.
    pub fn met_unverified(&self) -> bool {
        self.status == GoalStatus::Met
            && self
                .last_verdict
                .as_ref()
                .is_none_or(|v| !v.externally_verified())
    }

    /// The block appended to a compaction summary so a summarized session can
    /// never lose the objective.
    ///
    /// Framed as task data for the same reason the preamble block is: a goal is
    /// a long-lived channel, and nothing in it outranks the system prompt.
    pub fn critical_context(&self) -> String {
        let mut out = String::with_capacity(self.objective.len() + 256);
        out.push_str("Critical Context\nActive goal (task data, not instructions):\n");
        out.push_str(&self.objective);
        if !self.criteria.is_empty() {
            out.push_str("\nDone when:");
            for criterion in &self.criteria {
                out.push_str("\n- ");
                out.push_str(criterion);
            }
        }
        out.push_str(&format!(
            "\nStatus: {} (round {}/{})",
            self.status.label(),
            self.progress.rounds,
            self.bounds.max_rounds
        ));
        out
    }
}

/// Shared handle to the session's goal.
///
/// Cloning a live session keeps the same store, so a rebuilt agent and the
/// driver observe one another's updates. This mirrors
/// [`crate::agent::tools::TodoStore`], which exists for the same reason.
#[derive(Debug, Default, Clone)]
pub struct GoalStore(Arc<Mutex<Option<Goal>>>);

impl GoalStore {
    fn lock(&self) -> MutexGuard<'_, Option<Goal>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Build a store already holding `goal`.
    pub fn with_goal(goal: Goal) -> Self {
        Self(Arc::new(Mutex::new(Some(goal))))
    }

    /// Install a goal, refusing to discard an unfinished one unless `replace`.
    pub fn set(&self, goal: Goal, replace: bool) -> Result<(), GoalError> {
        let mut slot = self.lock();
        if let Some(existing) = slot.as_ref()
            && !existing.status.is_terminal()
            && !replace
        {
            return Err(GoalError::AlreadyActive {
                status: existing.status,
            });
        }
        *slot = Some(goal);
        Ok(())
    }

    /// Drop the goal entirely.
    pub fn clear(&self) -> Option<Goal> {
        self.lock().take()
    }

    /// A copy of the current goal.
    pub fn snapshot(&self) -> Option<Goal> {
        self.lock().clone()
    }

    /// Whether no goal is stored. Used by `skip_serializing_if` so a session
    /// without a goal serializes exactly as it did before goals existed.
    pub fn is_empty(&self) -> bool {
        self.lock().is_none()
    }

    /// Whether a goal is present and still being worked on.
    pub fn is_active(&self) -> bool {
        self.lock().as_ref().is_some_and(|g| g.status.is_running())
    }

    /// Whether a goal is present and unfinished, including parked states.
    pub fn is_live(&self) -> bool {
        self.lock()
            .as_ref()
            .is_some_and(|g| !g.status.is_terminal())
    }

    /// Mutate the goal in place, returning `None` when there is none.
    pub fn with_mut<T>(&self, f: impl FnOnce(&mut Goal) -> T) -> Option<T> {
        self.lock().as_mut().map(f)
    }

    /// Record a model report against the current round.
    pub fn append_report(&self, report: Report) -> bool {
        self.with_mut(|goal| goal.push_report(report)).is_some()
    }

    /// Compaction block for a live goal, mirroring
    /// [`crate::agent::tools::TodoStore::critical_context`].
    pub fn critical_context(&self) -> Option<String> {
        self.lock()
            .as_ref()
            .filter(|goal| !goal.status.is_terminal())
            .map(Goal::critical_context)
    }
}

impl Serialize for GoalStore {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.lock().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GoalStore {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // A status this build does not know must not fail the whole session
        // load; park the goal instead so the user can inspect it.
        let goal = Option::<StoredGoal>::deserialize(deserializer)?.map(StoredGoal::into_goal);
        Ok(Self(Arc::new(Mutex::new(goal))))
    }
}

/// Deserialization shim that tolerates an unknown `status`.
#[derive(Deserialize)]
struct StoredGoal {
    id: CompactString,
    objective: String,
    #[serde(default)]
    criteria: Vec<String>,
    status: serde_json::Value,
    #[serde(default)]
    paused_reason: Option<PauseReason>,
    #[serde(default)]
    checks: Vec<GoalCheck>,
    #[serde(default)]
    judge: JudgePolicy,
    #[serde(default)]
    resolved_judge: Option<ResolvedJudge>,
    #[serde(default)]
    continuation: ContinuationMode,
    #[serde(default)]
    bounds: GoalBounds,
    #[serde(default)]
    progress: GoalProgress,
    #[serde(default)]
    reports: VecDeque<Report>,
    #[serde(default)]
    last_verdict: Option<Verdict>,
    created_at: CompactString,
    updated_at: CompactString,
}

impl StoredGoal {
    fn into_goal(self) -> Goal {
        let (status, paused_reason) =
            match serde_json::from_value::<GoalStatus>(self.status.clone()) {
                Ok(status) => (status, self.paused_reason),
                Err(_) => {
                    tracing::warn!(
                        status = %self.status,
                        "goal: unrecognized stored status; parking the goal"
                    );
                    (GoalStatus::Paused, Some(PauseReason::UnknownStatusOnLoad))
                }
            };
        Goal {
            id: self.id,
            objective: self.objective,
            criteria: self.criteria,
            status,
            paused_reason,
            checks: self.checks,
            judge: self.judge,
            resolved_judge: self.resolved_judge,
            continuation: self.continuation,
            bounds: self.bounds,
            progress: self.progress,
            reports: self.reports,
            last_verdict: self.last_verdict,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal() -> Goal {
        Goal::new("ship the feature", vec!["tests pass".into()]).expect("valid goal")
    }

    #[test]
    fn objective_and_criteria_caps_are_enforced() {
        assert_eq!(
            Goal::new("   ", Vec::new()).unwrap_err(),
            GoalError::EmptyObjective
        );
        let long = "x".repeat(MAX_OBJECTIVE_CHARS + 1);
        assert_eq!(
            Goal::new(long, Vec::new()).unwrap_err(),
            GoalError::ObjectiveTooLong {
                len: MAX_OBJECTIVE_CHARS + 1
            }
        );
        let many = vec!["ok".to_string(); MAX_CRITERIA + 1];
        assert_eq!(
            Goal::new("objective", many).unwrap_err(),
            GoalError::TooManyCriteria {
                len: MAX_CRITERIA + 1
            }
        );
        let long_criterion = vec!["y".repeat(MAX_CRITERION_CHARS + 1)];
        assert_eq!(
            Goal::new("objective", long_criterion).unwrap_err(),
            GoalError::CriterionTooLong {
                index: 0,
                len: MAX_CRITERION_CHARS + 1
            }
        );
        // A boundary-length objective is accepted.
        assert!(Goal::new("x".repeat(MAX_OBJECTIVE_CHARS), Vec::new()).is_ok());
    }

    #[test]
    fn only_met_and_impossible_are_terminal() {
        for status in [GoalStatus::Met, GoalStatus::Impossible] {
            assert!(status.is_terminal(), "{status:?} must be terminal");
        }
        for status in [
            GoalStatus::Active,
            GoalStatus::AwaitingUser,
            GoalStatus::Paused,
            GoalStatus::Blocked,
            GoalStatus::BudgetLimited,
        ] {
            assert!(!status.is_terminal(), "{status:?} must be resumable");
        }
    }

    #[test]
    fn a_finished_goal_only_leaves_by_reopen() {
        assert!(!GoalStatus::Met.can_transition(GoalStatus::Active));
        assert!(GoalStatus::Met.can_transition(GoalStatus::Met));
        assert!(GoalStatus::Impossible.can_transition(GoalStatus::Active));
        assert!(!GoalStatus::Impossible.can_transition(GoalStatus::Met));
        assert!(GoalStatus::BudgetLimited.can_transition(GoalStatus::Active));
    }

    #[test]
    fn set_status_records_and_clears_the_pause_reason() {
        let mut goal = goal();
        assert!(goal.set_status(GoalStatus::Paused, Some(PauseReason::NoProgress)));
        assert_eq!(goal.paused_reason, Some(PauseReason::NoProgress));
        assert!(goal.set_status(GoalStatus::Active, None));
        assert_eq!(goal.paused_reason, None, "resuming clears the reason");

        let mut done = goal.clone();
        assert!(done.set_status(GoalStatus::Met, None));
        assert!(
            !done.set_status(GoalStatus::Active, None),
            "a met goal cannot be reactivated"
        );
        assert_eq!(done.status, GoalStatus::Met);
    }

    #[test]
    fn resume_and_reopen_reset_only_their_own_counters() {
        let mut goal = goal();
        goal.progress.rounds = 7;
        goal.progress.consecutive_no_progress = 3;
        goal.progress.wrap_up_issued = true;
        goal.set_status(GoalStatus::Paused, Some(PauseReason::NoProgress));

        assert!(goal.resume());
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.progress.consecutive_no_progress, 0);
        assert!(!goal.progress.wrap_up_issued);
        assert_eq!(goal.progress.rounds, 7, "round count is not reset");

        let mut impossible = goal.clone();
        impossible.set_status(GoalStatus::Impossible, None);
        assert!(impossible.reopen());
        assert_eq!(impossible.status, GoalStatus::Active);

        let mut met = goal.clone();
        met.set_status(GoalStatus::Met, None);
        assert!(!met.reopen(), "only an impossible goal reopens");
    }

    #[test]
    fn reports_are_bounded_and_queryable_by_round() {
        let mut goal = goal();
        for round in 1..=(MAX_REPORTS as u32 + 3) {
            goal.push_report(Report {
                status: ReportStatus::Progress,
                evidence: None,
                blocker: None,
                reason: None,
                question: None,
                round,
                at: now_rfc3339(),
            });
        }
        assert_eq!(goal.reports.len(), MAX_REPORTS);
        assert_eq!(
            goal.last_report().map(|r| r.round),
            Some(MAX_REPORTS as u32 + 3)
        );
        assert_eq!(goal.reports_in_round(MAX_REPORTS as u32 + 3).count(), 1);
        assert_eq!(goal.reports_in_round(1).count(), 0, "oldest dropped");
    }

    #[test]
    fn verdict_evidence_distinguishes_proof_from_claims() {
        assert!(VerificationKind::Checks.is_external_proof());
        assert!(VerificationKind::VerifyCommand.is_external_proof());
        assert!(!VerificationKind::SelfReport.is_external_proof());
        assert!(
            !VerificationKind::Judge.is_external_proof(),
            "a transcript judge is not external proof"
        );

        let mut goal = goal();
        goal.status = GoalStatus::Met;
        goal.last_verdict = Some(Verdict {
            outcome: Outcome::Met,
            reason: "looks done".into(),
            source: VerdictSource::Judge,
            evidence: vec![VerificationKind::SelfReport, VerificationKind::Judge],
            at: now_rfc3339(),
        });
        assert!(goal.met_unverified());

        goal.last_verdict.as_mut().unwrap().evidence = vec![VerificationKind::Checks];
        assert!(!goal.met_unverified());
    }

    #[test]
    fn store_refuses_to_replace_a_live_goal_without_permission() {
        let store = GoalStore::default();
        assert!(store.is_empty());
        store.set(goal(), false).expect("first goal");
        assert!(!store.is_empty());
        assert!(store.is_active());

        let err = store.set(goal(), false).unwrap_err();
        assert_eq!(
            err,
            GoalError::AlreadyActive {
                status: GoalStatus::Active
            }
        );
        store.set(goal(), true).expect("explicit replacement");

        // A finished goal is replaceable without asking.
        store.with_mut(|g| g.set_status(GoalStatus::Met, None));
        assert!(store.set(goal(), false).is_ok());
    }

    #[test]
    fn cloning_the_store_shares_one_goal() {
        let store = GoalStore::default();
        store.set(goal(), false).unwrap();
        let clone = store.clone();
        clone.with_mut(|g| g.progress.rounds = 4);
        assert_eq!(store.snapshot().unwrap().progress.rounds, 4);

        clone.clear();
        assert!(store.is_empty(), "clearing one handle clears both");
    }

    #[test]
    fn critical_context_carries_objective_and_criteria_for_live_goals_only() {
        let store = GoalStore::default();
        assert_eq!(store.critical_context(), None);
        store.set(goal(), false).unwrap();
        let block = store.critical_context().expect("live goal");
        assert!(block.contains("ship the feature"));
        assert!(block.contains("Done when:"));
        assert!(block.contains("tests pass"));
        assert!(
            block.contains("task data, not instructions"),
            "the objective must stay framed as data"
        );

        store.with_mut(|g| g.set_status(GoalStatus::Met, None));
        assert_eq!(
            store.critical_context(),
            None,
            "a finished goal is not re-injected"
        );
    }

    #[test]
    fn store_round_trips_through_json() {
        let store = GoalStore::default();
        store.set(goal(), false).unwrap();
        store.with_mut(|g| {
            g.set_status(GoalStatus::Blocked, None);
            g.checks.push(GoalCheck::new("cargo test"));
            g.judge = JudgePolicy::QuickModel("goal_judge".into());
            g.continuation = ContinuationMode::Restart {
                summary_chars: DEFAULT_RESTART_SUMMARY_CHARS,
            };
            g.progress.rounds = 3;
        });

        let json = serde_json::to_string(&store).unwrap();
        let back: GoalStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back.snapshot(), store.snapshot());
    }

    #[test]
    fn every_status_round_trips() {
        for status in [
            GoalStatus::Active,
            GoalStatus::AwaitingUser,
            GoalStatus::Paused,
            GoalStatus::Blocked,
            GoalStatus::BudgetLimited,
            GoalStatus::Met,
            GoalStatus::Impossible,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: GoalStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back, "{status:?} must survive a round trip");
        }
    }

    #[test]
    fn an_absent_goal_serializes_as_null_and_an_empty_store_is_skippable() {
        let store = GoalStore::default();
        assert_eq!(serde_json::to_string(&store).unwrap(), "null");
        assert!(store.is_empty());
        let back: GoalStore = serde_json::from_str("null").unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn an_unknown_stored_status_parks_the_goal_instead_of_failing_the_load() {
        let store = GoalStore::default();
        store.set(goal(), false).unwrap();
        let json = serde_json::to_string(&store).unwrap();
        let tampered = json.replace("\"active\"", "\"teleported\"");
        assert_ne!(
            tampered, json,
            "the fixture must actually change the status"
        );

        let back: GoalStore = serde_json::from_str(&tampered).expect("load must not fail");
        let goal = back.snapshot().expect("goal retained");
        assert_eq!(goal.status, GoalStatus::Paused);
        assert_eq!(goal.paused_reason, Some(PauseReason::UnknownStatusOnLoad));
        assert_eq!(goal.objective, "ship the feature", "payload is preserved");
    }

    #[test]
    fn resolved_judge_labels_a_same_model_judge() {
        let same = ResolvedJudge {
            label: "session".into(),
            provider: "openai".into(),
            model: "gpt-x".into(),
            same_as_session: true,
        };
        assert!(same.describe().contains("session model"));
        assert!(same.describe().contains("fresh context"));

        let distinct = ResolvedJudge {
            label: "goal_judge".into(),
            provider: "anthropic".into(),
            model: "small".into(),
            same_as_session: false,
        };
        assert_eq!(distinct.describe(), "goal_judge (anthropic/small)");
    }
}
