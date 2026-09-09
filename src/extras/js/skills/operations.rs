//! Explicit local-owner surfaces for the learned-skill lifecycle.
//!
//! These commands run before provider initialization. Purge uses the same
//! coordinator transaction/publication gate as lifecycle removals; compaction
//! never deletes raw events until their daily aggregates and watermark commit.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use rusqlite::OptionalExtension;

use super::admission::{
    AdmissionEvaluator, AuthenticatedHumanDecision, HumanReviewer, ReviewDecision, ReviewOutcome,
};
use super::coordinator::IndexCoordinator;
use super::embed::Embedder;
use super::feedback::{
    ActorKind, AuthenticatedActor, FeedbackCommand, FeedbackKind, FeedbackService,
};
use super::held_out::HeldOutSuiteDraft;
use super::lifecycle::LifecycleStatus;
use super::lifecycle::{
    CoordinatedLifecycle, EvidenceSnapshot, HumanApproval, LifecycleService,
    ReplacementTransitionRequest,
};
use super::privacy::Redactor;
use super::proposal::JsProposal;
use super::quarantine::{
    FeedbackAttribution, QuarantineEvidence, QuarantineExecutionError, QuarantineExecutor,
    QuarantinePolicy, QuarantineReason,
};
use super::retention::{
    CoordinatedRetention, DEFAULT_RAW_RETENTION_SECONDS, RetentionService, purge_preflight,
};
use super::store::{
    AdminIdentity, MAX_EVALUATION_ATTEMPTS, ProposalRecord, ProposalStatus, SkillStore,
    current_timestamp,
};
use crate::config::EmbeddingConfig;
use crate::extras::js::protocol::SkillProposalDraft;
use crate::paths::AppPaths;

pub(crate) struct FeedbackOperation<'a> {
    pub(crate) skill_id: &'a str,
    pub(crate) invocation_id: Option<&'a str>,
    pub(crate) kind: &'a str,
    pub(crate) reason_code: &'a str,
    pub(crate) idempotency_key: &'a str,
}

pub(crate) enum LibraryOperation<'a> {
    Import(&'a Path),
    InstallSeeds,
    Approve(&'a str),
    Reject(&'a str),
    Activate(&'a str),
    Promote(&'a str),
    Retire(&'a str),
    Reevaluate(&'a str),
    ListSuites,
    DisableSuite(&'a str),
}

/// Explicit privacy purge of one revision.
///
/// Purge deletes immutable bytes and re-roots dependants, so a non-terminal
/// target or one with dependants requires `force`.
pub(crate) struct PurgeOperation<'a> {
    pub(crate) skill_id: &'a str,
    pub(crate) force: bool,
}

static JSON_OUTPUT: AtomicBool = AtomicBool::new(false);

/// Select the operator output format for this process. Called once from the
/// CLI; every operator command renders through [`OperatorReport`].
pub(crate) fn set_json_output(enabled: bool) {
    JSON_OUTPUT.store(enabled, Ordering::Relaxed);
}

fn json_output() -> bool {
    JSON_OUTPUT.load(Ordering::Relaxed)
}

/// One structured operator result line.
///
/// Every learned-skill command reports the same shape — a command name, the
/// store's canonical identifier, the live status, and whichever of
/// `generation`/`idempotent` apply — so a script parses one format for all of
/// them instead of a different sentence per command.
struct OperatorReport {
    command: &'static str,
    fields: Vec<(&'static str, serde_json::Value)>,
}

impl OperatorReport {
    fn new(command: &'static str) -> Self {
        Self {
            command,
            fields: Vec::new(),
        }
    }

    fn with(mut self, name: &'static str, value: impl Into<serde_json::Value>) -> Self {
        self.fields.push((name, value.into()));
        self
    }

    fn render(&self, json: bool) -> String {
        if json {
            let mut object = serde_json::Map::new();
            object.insert(
                "command".to_string(),
                serde_json::Value::String(self.command.to_string()),
            );
            for (name, value) in &self.fields {
                object.insert((*name).to_string(), value.clone());
            }
            return serde_json::Value::Object(object).to_string();
        }
        let mut line = format!("learned-skill {}:", self.command);
        for (name, value) in &self.fields {
            line.push(' ');
            line.push_str(name);
            line.push('=');
            line.push_str(&render_text_value(value));
        }
        line
    }

    fn emit(self) {
        println!("{}", self.render(json_output()));
    }
}

/// Render one field for the human-readable line. Absent values print as the
/// same `-` placeholder the tabular listings already use.
fn render_text_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "-".to_string(),
        serde_json::Value::String(text) if text.is_empty() => "-".to_string(),
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) if items.is_empty() => "-".to_string(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(render_text_value)
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LearnedSkillPackage {
    proposal: SkillProposalDraft,
    held_out_suites: Vec<HeldOutSuiteDraft>,
}

const MAX_PACKAGE_BYTES: u64 = 256 * 1024;
const MAX_DIRECTORY_PACKAGES: usize = 32;
const SEED_PACKAGES: [(&str, &str); 5] = [
    (
        "json-parse",
        include_str!("../../../../assets/learned-skills/json-parse.json"),
    ),
    (
        "toml-parse",
        include_str!("../../../../assets/learned-skills/toml-parse.json"),
    ),
    (
        "csv-parse",
        include_str!("../../../../assets/learned-skills/csv-parse.json"),
    ),
    (
        "unified-diff",
        include_str!("../../../../assets/learned-skills/unified-diff.json"),
    ),
    (
        "table-format",
        include_str!("../../../../assets/learned-skills/table-format.json"),
    ),
];

#[derive(Debug, PartialEq)]
struct SkillUsageStats {
    skill_id: String,
    status: String,
    invocations: u64,
    direct_successes: u64,
    direct_failures: u64,
    last_used: Option<i64>,
    tasks_with: u64,
    passed_with: u64,
    baseline_tasks: u64,
    baseline_passes: u64,
    /// Tasks observed for this skill outside production (`MINI_AGENT_GYM`).
    ///
    /// Reported separately so gym traffic is visible without inflating the
    /// operator-facing utility columns, which count production rows only.
    gym_tasks: u64,
    user_positive: u64,
    user_negative: u64,
    declared_effect_methods: u64,
    estimated_round_trips_saved: u64,
}

/// Column header for [`SkillUsageStats::to_line`].
///
/// `tasks_with`, `passed_with` and `pass_rate_without` are derived from task
/// outcomes recorded against a `verify_command`: an operator who runs without
/// one records `no_verify_command` rows, and a turn that never touched the
/// workspace records `gate_skipped`. Neither carries a pass/fail signal, so
/// both are excluded and those three columns stay empty for that operator.
const SKILL_STATS_HEADER: &str = "id\tstatus\tinvocations\tsuccess\tlast_used_unix\t\
     tasks_with\tpassed_with\tpass_rate_without\tgym_tasks\tuser_positive\tuser_negative\t\
     declared_effect_methods\test_round_trips_saved";

impl SkillUsageStats {
    /// `None` when the ratio has no denominator, so "no observations" and "0%"
    /// stay distinguishable.
    fn success_percent(&self) -> Option<f64> {
        let terminals = self.direct_successes.saturating_add(self.direct_failures);
        (terminals != 0).then(|| self.direct_successes as f64 * 100.0 / terminals as f64)
    }

    /// `None` when no comparable task ran without the skill. An empty baseline
    /// is not a 0% baseline.
    fn pass_rate_without_percent(&self) -> Option<f64> {
        (self.baseline_tasks != 0)
            .then(|| self.baseline_passes as f64 * 100.0 / self.baseline_tasks as f64)
    }

    fn to_line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.skill_id,
            self.status,
            self.invocations,
            render_percent(self.success_percent()),
            self.last_used
                .map_or_else(|| "never".to_string(), |value| value.to_string()),
            self.tasks_with,
            self.passed_with,
            render_percent(self.pass_rate_without_percent()),
            self.gym_tasks,
            self.user_positive,
            self.user_negative,
            self.declared_effect_methods,
            self.estimated_round_trips_saved,
        )
    }
}

/// Render a percentage, or `n/a` where the denominator was zero.
fn render_percent(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |value| format!("{value:.1}%"))
}

fn estimate_round_trips_saved(direct_successes: u64, declared_effect_methods: u64) -> u64 {
    // Actual effect counts are intentionally not retained with skill telemetry.
    // This lower-bound proxy credits only additional distinct declared methods.
    direct_successes.saturating_mul(declared_effect_methods.saturating_sub(1))
}

/// Read per-revision usage and utility statistics.
///
/// The counts are computed with grouped set operations rather than correlated
/// subqueries per revision. The previous shape re-scanned every task outcome and
/// every baseline for each retained revision — and ran a further source-scope
/// search inside each baseline scan — so doubling the corpus multiplied the
/// query work roughly eightfold. Grouping once and joining the results per
/// revision keeps the exact source-kind/source-id and production matching, the
/// distinct-turn semantics, and the absent-baseline `n/a` behavior.
/// The statistics query, named so the plan regression can `EXPLAIN` exactly
/// what production runs.
const SKILL_STATS_SQL: &str = "WITH lost_turns AS (
             -- Grouped once rather than probed per outcome: no index covers
             -- (event_kind, turn_id) without a skill_id, so a correlated
             -- subquery here would scan every event for every outcome.
             SELECT DISTINCT turn_id, production
               FROM skill_events
              WHERE event_kind = 'observability_lost'
         ),
         qualified AS (
             SELECT outcome.evidence_id, outcome.turn_id, outcome.verify_passed,
                    outcome.source_kind, outcome.source_id, outcome.production
               FROM skill_task_outcomes AS outcome
               LEFT JOIN lost_turns
                 ON lost_turns.turn_id = outcome.turn_id
                AND lost_turns.production = outcome.production
              WHERE outcome.source_kind NOT IN ('no_verify_command', 'gate_skipped')
                -- A turn whose telemetry was lost or rejected is unknown, not
                -- a verified no-skill run: exclude it from utility and from the
                -- baseline comparison alike.
                AND outcome.evidence_complete = 1
                AND lost_turns.turn_id IS NULL
         ),
         linked AS (
             SELECT link.skill_id, qualified.turn_id, qualified.verify_passed,
                    qualified.source_kind, qualified.source_id, qualified.production
               FROM skill_task_outcome_links AS link
               JOIN qualified ON qualified.evidence_id = link.evidence_id
         ),
         observed AS (
             SELECT skill_id,
                    COUNT(DISTINCT CASE WHEN production = 1 THEN turn_id END)
                        AS tasks_with,
                    COUNT(DISTINCT CASE WHEN production = 1 AND verify_passed = 1
                                        THEN turn_id END) AS passed_with,
                    COUNT(DISTINCT CASE WHEN production = 0 THEN turn_id END)
                        AS gym_tasks
               FROM linked
              GROUP BY skill_id
         ),
         scopes AS (
             SELECT DISTINCT skill_id, source_kind, source_id
               FROM linked
              WHERE production = 1
         ),
         baselines AS (
             SELECT qualified.turn_id, qualified.verify_passed,
                    qualified.source_kind, qualified.source_id
               FROM qualified
              WHERE qualified.production = 1
                AND NOT EXISTS (
                    SELECT 1 FROM skill_task_outcome_links AS absent
                     WHERE absent.evidence_id = qualified.evidence_id
                )
         ),
         baseline_counts AS (
             SELECT scopes.skill_id,
                    COUNT(DISTINCT baselines.turn_id) AS baseline_tasks,
                    COUNT(DISTINCT CASE WHEN baselines.verify_passed = 1
                                        THEN baselines.turn_id END) AS baseline_passes
               FROM scopes
               JOIN baselines
                 ON baselines.source_kind = scopes.source_kind
                AND baselines.source_id IS scopes.source_id
              GROUP BY scopes.skill_id
         ),
         last_invoked AS (
             SELECT skill_id, MAX(created_at) AS last_used
               FROM skill_events
              WHERE event_kind = 'invoked'
              GROUP BY skill_id
         )
         SELECT revision.id, revision.status,
                COALESCE(stats.invoked_count, 0),
                COALESCE(stats.direct_success_count, 0),
                COALESCE(stats.direct_failure_count, 0),
                last_invoked.last_used,
                COALESCE(observed.tasks_with, 0),
                COALESCE(observed.passed_with, 0),
                COALESCE(baseline_counts.baseline_tasks, 0),
                COALESCE(baseline_counts.baseline_passes, 0),
                COALESCE(observed.gym_tasks, 0),
                COALESCE(stats.user_positive_count, 0),
                COALESCE(stats.user_negative_count, 0),
                revision.capability_json
           FROM skill_revisions AS revision
           LEFT JOIN skill_stats AS stats ON stats.skill_id = revision.id
           LEFT JOIN observed ON observed.skill_id = revision.id
           LEFT JOIN baseline_counts ON baseline_counts.skill_id = revision.id
           LEFT JOIN last_invoked ON last_invoked.skill_id = revision.id
          WHERE revision.identity_version = 2
          ORDER BY COALESCE(stats.invoked_count, 0) DESC, revision.id";

fn load_skill_stats(store: &SkillStore) -> anyhow::Result<Vec<SkillUsageStats>> {
    let mut statement = store.connection().prepare(SKILL_STATS_SQL)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, i64>(9)?,
            row.get::<_, i64>(10)?,
            row.get::<_, i64>(11)?,
            row.get::<_, i64>(12)?,
            row.get::<_, String>(13)?,
        ))
    })?;
    rows.map(|row| {
        let (
            skill_id,
            status,
            invocations,
            successes,
            failures,
            last_used,
            tasks_with,
            passed_with,
            baseline_tasks,
            baseline_passes,
            gym_tasks,
            user_positive,
            user_negative,
            capability_json,
        ) = row?;
        let declared_effect_methods = serde_json::from_str::<serde_json::Value>(&capability_json)
            .ok()
            .and_then(|value| value.pointer("/manifest/grants")?.as_array().map(Vec::len))
            .unwrap_or(0) as u64;
        let direct_successes = u64::try_from(successes).unwrap_or(0);
        Ok(SkillUsageStats {
            skill_id,
            status,
            invocations: u64::try_from(invocations).unwrap_or(0),
            direct_successes,
            direct_failures: u64::try_from(failures).unwrap_or(0),
            last_used,
            tasks_with: u64::try_from(tasks_with).unwrap_or(0),
            passed_with: u64::try_from(passed_with).unwrap_or(0),
            baseline_tasks: u64::try_from(baseline_tasks).unwrap_or(0),
            baseline_passes: u64::try_from(baseline_passes).unwrap_or(0),
            gym_tasks: u64::try_from(gym_tasks).unwrap_or(0),
            user_positive: u64::try_from(user_positive).unwrap_or(0),
            user_negative: u64::try_from(user_negative).unwrap_or(0),
            declared_effect_methods,
            estimated_round_trips_saved: estimate_round_trips_saved(
                direct_successes,
                declared_effect_methods,
            ),
        })
    })
    .collect()
}

pub(crate) fn print_skill_stats(paths: &AppPaths) -> anyhow::Result<()> {
    let store = SkillStore::open_at(paths).context("failed to open learned-skill store")?;
    let rows = load_skill_stats(&store).context("failed to read learned-skill usage")?;
    println!("{SKILL_STATS_HEADER}");
    for row in &rows {
        println!("{}", row.to_line());
    }
    let invocations = rows.iter().map(|row| row.invocations).sum::<u64>();
    let saved = rows
        .iter()
        .map(|row| row.estimated_round_trips_saved)
        .sum::<u64>();
    let gym = rows.iter().map(|row| row.gym_tasks).sum::<u64>();
    let positive = rows.iter().map(|row| row.user_positive).sum::<u64>();
    let negative = rows.iter().map(|row| row.user_negative).sum::<u64>();
    println!("total\t-\t{invocations}\t-\t-\t-\t-\t-\t{gym}\t{positive}\t{negative}\t-\t{saved}");
    Ok(())
}

/// One learned-skill proposal that is still awaiting an operator decision.
#[derive(Debug, PartialEq, Eq)]
struct ProposalQueueRow {
    proposal_id: String,
    skill_id: String,
    status: String,
    reason_code: Option<String>,
    report_id: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl ProposalQueueRow {
    fn to_line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.proposal_id,
            self.skill_id,
            self.status,
            self.reason_code.as_deref().unwrap_or("-"),
            self.report_id.as_deref().unwrap_or("-"),
            self.created_at,
            self.updated_at,
        )
    }
}

/// Read every proposal in a non-terminal status. Rejected proposals are
/// terminal and are deliberately excluded: no operator action can advance them.
fn load_proposal_queue(store: &SkillStore) -> anyhow::Result<Vec<ProposalQueueRow>> {
    let mut statement = store.connection().prepare(
        "SELECT proposal_id, skill_id, status, reason_code, report_id,
                created_at, updated_at
           FROM skill_proposals
          WHERE status IN (
              'pending', 'evaluating', 'verified', 'awaiting_approval',
              'approved', 'deferred'
          )
          ORDER BY created_at, proposal_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(ProposalQueueRow {
            proposal_id: row.get(0)?,
            skill_id: row.get(1)?,
            status: row.get(2)?,
            reason_code: row.get(3)?,
            report_id: row.get(4)?,
            created_at: row.get(5)?,
            updated_at: row.get(6)?,
        })
    })?;
    rows.map(|row| row.map_err(anyhow::Error::from)).collect()
}

/// Read one proposal by identifier in *any* status.
///
/// The queue listing deliberately omits terminal proposals, which left a
/// rejected or deferred admission outcome — its `reason_code` and the
/// `report_id` that carries the evidence — invisible to the operator.
fn load_proposal(
    store: &SkillStore,
    proposal_id: &str,
) -> anyhow::Result<Option<ProposalQueueRow>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT proposal_id, skill_id, status, reason_code, report_id,
                    created_at, updated_at
               FROM skill_proposals
              WHERE proposal_id = ?1 OR skill_id = ?1",
            [proposal_id],
            |row| {
                Ok(ProposalQueueRow {
                    proposal_id: row.get(0)?,
                    skill_id: row.get(1)?,
                    status: row.get(2)?,
                    reason_code: row.get(3)?,
                    report_id: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            },
        )
        .optional()?)
}

/// Print the final admission outcome of one proposal.
pub(crate) fn print_proposal(paths: &AppPaths, proposal_id: &str) -> anyhow::Result<()> {
    let store = SkillStore::open_at(paths).context("failed to open learned-skill store")?;
    let row = load_proposal(&store, proposal_id)
        .context("failed to read the learned-skill proposal")?
        .with_context(|| format!("learned-skill proposal not found: {proposal_id}"))?;
    let revision_status = store.revision_status(&row.skill_id)?;
    // A rejected or deferred proposal is a decision the operator has to be
    // able to see, so it is logged as well as printed.
    if matches!(row.status.as_str(), "rejected" | "deferred") {
        tracing::warn!(
            proposal_id = %row.proposal_id,
            skill_id = %row.skill_id,
            status = %row.status,
            reason_code = row.reason_code.as_deref().unwrap_or(""),
            report_id = row.report_id.as_deref().unwrap_or(""),
            "learned-skill proposal reached a non-approvable admission outcome"
        );
    }
    OperatorReport::new("proposal")
        .with("id", row.skill_id.clone())
        .with("proposal_id", row.proposal_id.clone())
        .with("status", row.status.clone())
        .with("revision_status", revision_status)
        .with("reason_code", row.reason_code.clone())
        .with("report_id", row.report_id.clone())
        .with("created_at", row.created_at)
        .with("updated_at", row.updated_at)
        .emit();
    Ok(())
}

pub(crate) fn print_proposal_queue(paths: &AppPaths) -> anyhow::Result<()> {
    let store = SkillStore::open_at(paths).context("failed to open learned-skill store")?;
    let rows = load_proposal_queue(&store).context("failed to read learned-skill proposals")?;
    println!(
        "proposal_id\tskill_id\tstatus\treason_code\treport_id\tcreated_at_unix\tupdated_at_unix"
    );
    for row in &rows {
        println!("{}", row.to_line());
    }
    println!("total\t{}\t-\t-\t-\t-\t-", rows.len());
    Ok(())
}

pub(crate) fn run(
    purge: Option<PurgeOperation<'_>>,
    compact: bool,
    feedback: Option<FeedbackOperation<'_>>,
    library: Option<LibraryOperation<'_>>,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    if let Some(operation) = library {
        return run_library_operation(operation, paths, embedding);
    }
    if let Some(operation) = purge {
        return purge_skill(operation, paths, embedding);
    }

    if compact {
        let now = current_timestamp().context("failed to resolve compaction timestamp")?;
        let cutoff = now.saturating_sub(DEFAULT_RAW_RETENTION_SECONDS);
        let mut store = SkillStore::open_at(paths).context("failed to open learned-skill store")?;
        let report = RetentionService::new(&mut store)
            .compact_before(cutoff, 1, now)
            .context("learned-skill telemetry compaction failed")?;
        OperatorReport::new("compact")
            .with("events", report.compacted_events as u64)
            .with("through_event_id", report.through_event_id)
            .emit();
        return Ok(());
    }

    if let Some(feedback) = feedback {
        submit_feedback(feedback, paths, embedding)?;
    }
    Ok(())
}

/// Purge one revision's immutable bytes after an explicit lifecycle and
/// reference guard.
///
/// The guard is deliberately loud rather than silent: a non-terminal target or
/// one with dependent revisions is refused unless the operator passed the force
/// flag, and a forced purge names every revision it re-roots. Re-rooting is not
/// cosmetic — a canary replacement whose predecessor is purged becomes a
/// lineage root and would then pass `--activate-learned-skill` with no
/// replacement evidence behind it.
fn purge_skill(
    operation: PurgeOperation<'_>,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    let PurgeOperation { skill_id, force } = operation;
    let store = SkillStore::open_at(paths).context("failed to open learned-skill store")?;
    let plan = purge_preflight(&store, skill_id)
        .context("failed to inspect the learned-skill purge target")?;
    drop(store);
    let status = plan.status.clone();
    let rerooted = plan
        .rerooted_ids()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if plan.requires_force() && !force {
        if !plan.is_terminal() {
            anyhow::bail!(
                "learned skill {skill_id} is {} and is not in a terminal lifecycle status; \
                 re-run with --purge-learned-skill-force to delete it{}",
                status.as_deref().unwrap_or("absent"),
                dependant_suffix(&rerooted)
            );
        }
        anyhow::bail!(
            "purging learned skill {skill_id} would re-root {} dependent revision(s) ({}); \
             re-run with --purge-learned-skill-force to accept that",
            rerooted.len(),
            rerooted.join(",")
        );
    }
    let embedder = Arc::new(
        Embedder::from_config(embedding)
            .context("failed to initialize learned-skill index metadata")?,
    );
    let coordinator = IndexCoordinator::open(paths, embedder)
        .context("failed to open learned-skill index coordinator")?;
    let now = current_timestamp().context("failed to resolve purge timestamp")?;
    let (generation, publication) = CoordinatedRetention::new(&coordinator)
        .privacy_purge(skill_id, "local_operator_request", now)
        .context("learned-skill privacy purge failed")?;
    OperatorReport::new("purge")
        .with("id", skill_id)
        .with("status", "purged")
        .with("previous_status", status.clone())
        .with("generation", generation)
        .with("removal_only", publication.removal_only)
        // A tombstoned target has no revision row left to purge, so a repeat
        // purge is an acknowledged replay rather than a second deletion.
        .with("idempotent", status.is_none())
        .with(
            "rerooted",
            rerooted
                .iter()
                .map(|id| serde_json::Value::String(id.clone()))
                .collect::<Vec<_>>(),
        )
        .emit();
    Ok(())
}

fn dependant_suffix(rerooted: &[String]) -> String {
    if rerooted.is_empty() {
        String::new()
    } else {
        format!(
            " (this would also re-root {} dependent revision(s): {})",
            rerooted.len(),
            rerooted.join(",")
        )
    }
}

fn run_library_operation(
    operation: LibraryOperation<'_>,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    match operation {
        LibraryOperation::Import(path) => import_path(path, paths, embedding),
        LibraryOperation::InstallSeeds => install_seeds(paths, embedding),
        LibraryOperation::Approve(id) => review_proposal(id, true, paths, embedding),
        LibraryOperation::Reject(id) => review_proposal(id, false, paths, embedding),
        LibraryOperation::Activate(id) => activate_skill(id, paths, embedding),
        LibraryOperation::Promote(id) => promote_replacement_skill(id, paths, embedding),
        LibraryOperation::Retire(id) => retire_skill(id, paths, embedding),
        LibraryOperation::Reevaluate(id) => reevaluate_skill(id, paths),
        LibraryOperation::ListSuites => {
            let store = SkillStore::open_at(paths)?;
            suite_listing_report(&store)?.emit();
            Ok(())
        }
        LibraryOperation::DisableSuite(id) => {
            let mut store = SkillStore::open_at(paths)?;
            let admin = AdminIdentity::authenticated("local-owner")?;
            let changed = store.disable_held_out_suite(Some(&admin), id)?;
            OperatorReport::new("disable-suite")
                .with("id", id)
                .with("enabled", false)
                .with("idempotent", !changed)
                .emit();
            Ok(())
        }
    }
}

fn suite_listing_report(store: &SkillStore) -> anyhow::Result<OperatorReport> {
    let suites: Vec<_> = store
        .held_out_suite_states()?
        .into_iter()
        .map(|(id, enabled)| serde_json::json!({ "id": id, "enabled": enabled }))
        .collect();
    Ok(OperatorReport::new("list-suites").with("suites", suites))
}

/// Requeue a parked proposal or refresh an unapproved evaluation report.
///
/// A proposal reaches `verified` with `held_out_suite_required` when no enabled
/// suite matched it, and `deferred` when the verification infrastructure was
/// unavailable or its attempt budget ran out. An `awaiting_approval` report
/// also needs reevaluation when the verifier or trusted corpus changes.
fn reevaluate_skill(skill_id: &str, paths: &AppPaths) -> anyhow::Result<()> {
    let mut store = SkillStore::open_at(paths).context("failed to open learned-skill store")?;
    let proposal = store
        .get_proposal(skill_id)?
        .context("learned-skill proposal not found")?;
    let status = proposal_status(proposal.status);
    let reason = proposal.reason_code.clone();

    let admin = AdminIdentity::authenticated("local-owner")?;
    store
        .request_blocked_reevaluation(
            Some(&admin),
            &proposal.proposal_id,
            proposal.row_version,
            current_timestamp()?,
        )
        .with_context(|| {
            format!(
                "learned-skill re-evaluation requires a proposal awaiting approval or parked as \
                 verified with held_out_suite_required or as deferred; proposal {} is {status}{}",
                proposal.proposal_id,
                reason
                    .as_deref()
                    .map(|code| format!(" ({code})"))
                    .unwrap_or_default()
            )
        })?;
    OperatorReport::new("reevaluate")
        .with("id", proposal.skill_id.as_str())
        .with("proposal_id", proposal.proposal_id.as_str())
        .with("previous_status", status)
        .with("previous_reason", reason.as_deref().unwrap_or("-"))
        .with("status", "pending")
        .emit();
    Ok(())
}

/// Import every bundled seed, reporting each one independently.
///
/// Seeds are unrelated packages, so one failure must not hide the rest: a seed
/// that was previously rejected, purged or bound to a moved predecessor is
/// reported and skipped. The command is idempotent — re-running it over an
/// already-imported library reports each seed's live state — and fails only
/// when *no* seed reached an importable state.
fn install_seeds(paths: &AppPaths, embedding: Option<&EmbeddingConfig>) -> anyhow::Result<()> {
    let mut imported = 0usize;
    let mut failed = 0usize;
    let mut first_error: Option<String> = None;
    for (name, source) in SEED_PACKAGES {
        let outcome = serde_json::from_str::<LearnedSkillPackage>(source)
            .with_context(|| format!("bundled learned-skill seed {name} is invalid"))
            .and_then(|package| {
                validate_package(&package, name)?;
                import_package(package, paths, embedding, name)
            });
        match outcome {
            Ok(report) => {
                imported += 1;
                report
                    .into_operator_report("install-seed")
                    .with("seed", name)
                    .emit();
            }
            Err(error) => {
                failed += 1;
                let detail = format!("{error:#}");
                OperatorReport::new("install-seed")
                    .with("seed", name)
                    .with("status", "failed")
                    .with("error", detail.clone())
                    .emit();
                first_error.get_or_insert(detail);
            }
        }
    }
    OperatorReport::new("install-seeds")
        .with("imported", imported as u64)
        .with("failed", failed as u64)
        .emit();
    if imported == 0 {
        anyhow::bail!(
            "no bundled learned-skill seed reached an importable state ({failed} failed); \
             first failure: {}",
            first_error.unwrap_or_else(|| "unknown".to_string())
        );
    }
    Ok(())
}

/// Retire an active learned skill by explicit operator action.
///
/// Unlike `--purge-learned-skill` this keeps the revision, its lineage and its
/// audit: it is the administrative disable the lifecycle defines, published
/// through the same coordinated gate as every other visibility change.
fn retire_skill(
    skill_id: &str,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    let store = SkillStore::open_at(paths).context("failed to open learned-skill store")?;
    let status = store
        .revision_status(skill_id)
        .context("failed to read the learned-skill revision")?
        .context("learned-skill revision not found")?;
    let row_version = store
        .revision_row_version(skill_id)
        .context("failed to read the learned-skill row version")?
        .context("learned-skill revision not found")?;
    drop(store);
    if status == "retired" {
        OperatorReport::new("retire")
            .with("id", skill_id)
            .with("status", status)
            .with("idempotent", true)
            .emit();
        return Ok(());
    }
    if status != "active" {
        anyhow::bail!(
            "learned-skill retirement requires an active revision; \
             revision {skill_id} is {status}"
        );
    }
    let embedder = Arc::new(Embedder::from_config(embedding)?);
    let coordinator = IndexCoordinator::open(paths, embedder)
        .context("failed to open learned-skill index coordinator")?;
    let generation = coordinator
        .retire_and_publish(skill_id, row_version)
        .context("learned-skill retirement failed")?;
    let store = SkillStore::open_at(paths)?;
    let status = store
        .revision_status(skill_id)?
        .context("learned-skill revision disappeared after retirement")?;
    OperatorReport::new("retire")
        .with("id", skill_id)
        .with("status", status)
        .with("generation", generation)
        .with("idempotent", false)
        .emit();
    Ok(())
}

fn import_path(
    path: &Path,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect learned-skill package {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("learned-skill import path must not be a symbolic link");
    }
    if metadata.is_file() {
        let label = path.display().to_string();
        let package = read_package(path)?;
        validate_package(&package, &label)?;
        import_package(package, paths, embedding, &label)?
            .into_operator_report("import")
            .emit();
        return Ok(());
    }
    if !metadata.is_dir() {
        anyhow::bail!("learned-skill import path must be a JSON file or directory");
    }
    let entries = std::fs::read_dir(path)
        .with_context(|| format!("failed to read learned-skill directory {}", path.display()))?;
    let mut packages = Vec::<PathBuf>::new();
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to inspect an entry in {}", path.display()))?;
        let file_type = entry.file_type().with_context(|| {
            format!(
                "failed to inspect learned-skill package {}",
                entry.path().display()
            )
        })?;
        if file_type.is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
        {
            packages.push(entry.path());
            if packages.len() > MAX_DIRECTORY_PACKAGES {
                anyhow::bail!("learned-skill directory must contain 1 to 32 regular JSON files");
            }
        }
    }
    packages.sort();
    if packages.is_empty() {
        anyhow::bail!("learned-skill directory must contain 1 to 32 regular JSON files");
    }
    let packages = packages
        .into_iter()
        .map(|package_path| {
            let label = package_path.display().to_string();
            let package = read_package(&package_path)?;
            validate_package(&package, &label)?;
            Ok((label, package))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    for (label, package) in packages {
        import_package(package, paths, embedding, &label)?
            .into_operator_report("import")
            .emit();
    }
    Ok(())
}

fn validate_package(package: &LearnedSkillPackage, label: &str) -> anyhow::Result<()> {
    if package.held_out_suites.is_empty() {
        anyhow::bail!("learned-skill package {label} requires at least one held-out suite");
    }
    JsProposal::try_from(package.proposal.clone())
        .context("learned-skill proposal shape is invalid")?
        .validate_and_canonicalize()
        .context("learned-skill proposal identity is invalid")?;
    for suite in &package.held_out_suites {
        suite
            .validate()
            .context("learned-skill held-out baseline is invalid")?;
    }
    Ok(())
}

fn read_package(path: &Path) -> anyhow::Result<LearnedSkillPackage> {
    let before = crate::fs::checked_path_metadata(path)
        .with_context(|| format!("failed to inspect learned-skill package {}", path.display()))?;
    if !before.is_file() || before.file_type().is_symlink() {
        anyhow::bail!("learned-skill package must be a regular file");
    }
    #[cfg(test)]
    if let Some(action) = tests::BEFORE_PACKAGE_OPEN.with(|slot| slot.borrow_mut().take()) {
        action();
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open learned-skill package {}", path.display()))?;
    let opened = crate::fs::checked_file_metadata(&file)?;
    if !opened.is_file() || opened.file_type().is_symlink() {
        anyhow::bail!("learned-skill package must be a regular file");
    }
    crate::fs::ensure_same_file(path, &before, &opened)?;
    let after = crate::fs::checked_path_metadata(path)?;
    crate::fs::ensure_same_file(path, &opened, &after)?;
    let mut bytes = Vec::new();
    file.take(MAX_PACKAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read learned-skill package {}", path.display()))?;
    if bytes.len() as u64 > MAX_PACKAGE_BYTES {
        anyhow::bail!("learned-skill package exceeds 256 KiB");
    }
    serde_json::from_slice(&bytes)
        .with_context(|| format!("learned-skill package {} is invalid", path.display()))
}

/// What one imported package reached, sourced from the store rather than from
/// the caller's argument.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportReport {
    skill_id: String,
    proposal_id: String,
    status: ProposalStatus,
    reason_code: Option<String>,
    report_id: Option<String>,
    /// True when the proposal already existed in this exact form, so the
    /// import changed nothing.
    idempotent: bool,
    /// Set when the proposal is durably queued for a later evaluation attempt.
    next_attempt_at: Option<i64>,
    /// Set when an unrelated proposal held the head of the due queue, so this
    /// command deliberately declined to evaluate anything.
    blocked_by: Option<String>,
}

impl ImportReport {
    fn into_operator_report(self, command: &'static str) -> OperatorReport {
        OperatorReport::new(command)
            .with("id", self.skill_id)
            .with("proposal_id", self.proposal_id)
            .with("status", proposal_status(self.status))
            .with("reason_code", self.reason_code)
            .with("report_id", self.report_id)
            .with("idempotent", self.idempotent)
            .with("next_attempt_at", self.next_attempt_at)
            .with("blocked_by", self.blocked_by)
    }
}

/// Wall-clock budget for the bounded evaluation wait an operator import
/// performs before reporting the proposal as queued for a later attempt.
const IMPORT_EVALUATION_BUDGET: Duration = Duration::from_secs(120);
/// Poll cadence while another worker owns the evaluation lease.
const IMPORT_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Longest single sleep, so a long retry backoff still respects the budget.
const IMPORT_MAX_SLEEP: Duration = Duration::from_secs(2);
/// How long the import waits for an unrelated due proposal to clear before it
/// reports its own proposal as queued instead of evaluating that proposal.
const FOREIGN_HEAD_GRACE: Duration = Duration::from_secs(2);

/// The proposal `AdmissionEvaluator::evaluate_next` would claim right now.
///
/// The operator import drives evaluation only while its *own* proposal is at
/// the head of the due queue: `evaluate_next` claims the oldest due proposal of
/// any identity, so calling it unconditionally would spend an unrelated,
/// agent-originated proposal's retry budget inside an operator command.
fn due_proposal_head(store: &SkillStore, now: i64) -> anyhow::Result<Option<String>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT proposal_id FROM skill_proposals
              WHERE attempt_count < ?1
                AND (
                  (status = 'pending' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
                  OR (status = 'evaluating' AND lease_expires_at <= ?2)
                )
              ORDER BY proposed_at, proposal_id
              LIMIT 1",
            rusqlite::params![MAX_EVALUATION_ATTEMPTS, now],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

/// Refuse an import whose enqueue cannot succeed *before* held-out baselines
/// are written.
///
/// `enqueue_proposal` and the held-out suite import each own their transaction,
/// so a failing enqueue after a successful suite import would leave trusted
/// baselines behind for a skill that was never queued. A single transaction
/// spanning both would need a store-side API; until then this pre-flight
/// removes every failure the enqueue can predict, and names the predecessor's
/// observed state instead of one message for absent and ineligible alike.
fn preflight_enqueue(
    store: &SkillStore,
    artifact: &super::SkillArtifact,
    predecessor_id: Option<&str>,
) -> anyhow::Result<()> {
    let tombstoned = store
        .connection()
        .query_row(
            "SELECT 1 FROM skill_tombstones WHERE id = ?",
            [&artifact.id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if tombstoned {
        anyhow::bail!(
            "learned skill {} was privacy-purged and cannot be re-proposed",
            artifact.id
        );
    }
    if let Some(predecessor_id) = predecessor_id {
        let status: Option<String> = store
            .connection()
            .query_row(
                "SELECT status FROM skill_revisions WHERE id = ?",
                [predecessor_id],
                |row| row.get(0),
            )
            .optional()?;
        match status.as_deref() {
            Some("active" | "canary" | "quarantined") => {}
            Some(observed) => anyhow::bail!(
                "learned-skill predecessor {predecessor_id} is {observed}; a replacement \
                 requires an active, canary, or quarantined immutable revision"
            ),
            None => anyhow::bail!(
                "learned-skill predecessor {predecessor_id} is absent from the store; \
                 import the predecessor before its replacement"
            ),
        }
    }
    if let Some(existing) = store.get(&artifact.id)?
        && existing != *artifact
    {
        anyhow::bail!("identity collision for learned skill {}", artifact.id);
    }
    if let Some(record) = store.get_proposal(&artifact.id)?
        && record.predecessor_id.as_deref() != predecessor_id
    {
        anyhow::bail!(
            "learned skill {} is already proposed against predecessor {}; an existing \
             proposal cannot be rebound to {}",
            artifact.id,
            record.predecessor_id.as_deref().unwrap_or("none"),
            predecessor_id.unwrap_or("none")
        );
    }
    Ok(())
}

fn import_package(
    package: LearnedSkillPackage,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
    label: &str,
) -> anyhow::Result<ImportReport> {
    import_package_within(package, paths, embedding, label, IMPORT_EVALUATION_BUDGET)
}

fn import_package_within(
    package: LearnedSkillPackage,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
    label: &str,
    budget: Duration,
) -> anyhow::Result<ImportReport> {
    validate_package(&package, label)?;
    let LearnedSkillPackage {
        proposal,
        held_out_suites,
    } = package;
    let predecessor_id = proposal.predecessor_id.clone();
    let artifact = JsProposal::try_from(proposal)
        .context("learned-skill proposal shape is invalid")?
        .validate_and_canonicalize()
        .context("learned-skill proposal identity is invalid")?;
    let now = current_timestamp().context("failed to resolve import timestamp")?;
    let admin = AdminIdentity::authenticated("local-owner")?;
    let mut store = SkillStore::open_at(paths).context("failed to open learned-skill store")?;
    preflight_enqueue(&store, &artifact, predecessor_id.as_deref())
        .context("learned-skill proposal cannot be enqueued")?;
    for suite in held_out_suites {
        suite
            .import(&mut store, &admin, now)
            .context("failed to import learned-skill held-out baseline")?;
    }
    let already_present = store.get_proposal(&artifact.id)?.is_some();
    let queued = store
        .enqueue_proposal(&artifact, predecessor_id.as_deref(), now)
        .context("failed to enqueue learned-skill proposal")?;
    drop(store);

    let mut evaluator = AdmissionEvaluator::new(
        SkillStore::open_at(paths)?,
        std::sync::Arc::new(Embedder::from_config(embedding)?),
        format!("local-import-{}", uuid::Uuid::new_v4()),
    )?;
    let existing = SkillStore::open_at(paths)?
        .get_proposal(&queued.proposal_id)?
        .context("imported proposal disappeared")?;
    if existing.status == ProposalStatus::Verified
        && existing.reason_code.as_deref() == Some("held_out_suite_required")
    {
        evaluator
            .request_reevaluation(&queued.proposal_id, &admin, current_timestamp()?)
            .context("failed to requeue proposal after held-out baseline import")?;
    }

    // Bounded by wall clock rather than by an iteration count: a retryable
    // failure reschedules the proposal with exponential backoff, and a fixed
    // number of immediate iterations would spin past every one of them and
    // then report a queued proposal as a failure.
    let deadline = std::time::Instant::now() + budget;
    let mut infrastructure_attempts = existing.infrastructure_attempt_count;
    let mut blocked_since: Option<std::time::Instant> = None;
    loop {
        let store = SkillStore::open_at(paths)?;
        let current = store
            .get_proposal(&queued.proposal_id)?
            .context("imported proposal disappeared")?;
        let now = current_timestamp()?;
        let head = due_proposal_head(&store, now)?;
        drop(store);
        match current.status {
            ProposalStatus::AwaitingApproval | ProposalStatus::Approved => {
                return Ok(import_report(&current, already_present));
            }
            ProposalStatus::Rejected | ProposalStatus::Verified => {
                anyhow::bail!(
                    "learned-skill verification did not reach awaiting approval: id={} status={}{}",
                    current.skill_id,
                    proposal_status(current.status),
                    current
                        .reason_code
                        .as_deref()
                        .map(|reason| format!(" reason={reason}"))
                        .unwrap_or_default()
                );
            }
            ProposalStatus::Deferred => {
                anyhow::bail!(
                    "learned-skill verification infrastructure is unavailable on this host: \
                     id={} status=deferred{}",
                    current.skill_id,
                    current
                        .reason_code
                        .as_deref()
                        .map(|reason| format!(" reason={reason}"))
                        .unwrap_or_default()
                );
            }
            ProposalStatus::Pending | ProposalStatus::Evaluating => {}
        }
        if std::time::Instant::now() >= deadline {
            return Ok(scheduled_report(&current, already_present, now, None));
        }
        if current.next_attempt_at.is_some_and(|due| due > now) {
            // The proposal is durably queued behind its own retry backoff.
            // Sleeping to its next attempt is the whole point of the bound.
            std::thread::sleep(sleep_until_due(current.next_attempt_at, now, deadline));
            continue;
        }
        match head.as_deref() {
            Some(head) if head == queued.proposal_id => {}
            Some(other) => {
                // Another proposal is due first. `evaluate_next` would claim
                // it, so this operator command waits briefly and then reports
                // its own proposal as queued rather than spending an unrelated
                // proposal's retry budget.
                if blocked_since
                    .get_or_insert_with(std::time::Instant::now)
                    .elapsed()
                    >= FOREIGN_HEAD_GRACE
                {
                    return Ok(scheduled_report(
                        &current,
                        already_present,
                        now,
                        Some(other.to_string()),
                    ));
                }
                std::thread::sleep(
                    IMPORT_POLL_INTERVAL
                        .min(deadline.saturating_duration_since(std::time::Instant::now())),
                );
                continue;
            }
            None => return Ok(scheduled_report(&current, already_present, now, None)),
        }
        blocked_since = None;
        match evaluator.evaluate_next(now) {
            Ok(Some(_)) => {}
            // Nothing was claimable after all: report the queued proposal
            // rather than spinning on an empty queue.
            Ok(None) => return Ok(scheduled_report(&current, already_present, now, None)),
            Err(error) => {
                let observed = SkillStore::open_at(paths)?
                    .get_proposal(&queued.proposal_id)?
                    .context("imported proposal disappeared")?;
                // An uncontained host fails the same way on every attempt, so
                // report the containment reason immediately instead of looping
                // until the budget expires.
                if observed.infrastructure_attempt_count > infrastructure_attempts {
                    anyhow::bail!(
                        "learned-skill verification could not run in a contained worker: \
                         id={} {error}",
                        observed.skill_id
                    );
                }
                infrastructure_attempts = observed.infrastructure_attempt_count;
                tracing::warn!(error = %error, "learned-skill evaluation will retry");
            }
        }
    }
}

/// Report a proposal that is durably queued for a later attempt. This is not
/// a failure: a session admission worker picks it up, and re-running the import
/// is idempotent.
fn scheduled_report(
    current: &ProposalRecord,
    already_present: bool,
    now: i64,
    blocked_by: Option<String>,
) -> ImportReport {
    let mut report = import_report(current, already_present);
    report.next_attempt_at = report.next_attempt_at.or(Some(now));
    report.blocked_by = blocked_by;
    report
}

fn import_report(current: &ProposalRecord, already_present: bool) -> ImportReport {
    ImportReport {
        skill_id: current.skill_id.clone(),
        proposal_id: current.proposal_id.clone(),
        status: current.status,
        reason_code: current.reason_code.clone(),
        report_id: current.report_id.clone(),
        idempotent: already_present,
        next_attempt_at: current.next_attempt_at,
        blocked_by: None,
    }
}

/// Sleep long enough for the proposal to become due, without overshooting the
/// import's own deadline.
fn sleep_until_due(
    next_attempt_at: Option<i64>,
    now: i64,
    deadline: std::time::Instant,
) -> Duration {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let wait = next_attempt_at
        .map(
            |due| Duration::from_secs(due.saturating_sub(now).clamp(0, i64::from(u32::MAX)) as u64),
        )
        .unwrap_or(IMPORT_POLL_INTERVAL)
        .max(IMPORT_POLL_INTERVAL)
        .min(IMPORT_MAX_SLEEP);
    wait.min(remaining)
}

struct LocalOwnerReviewer {
    now: i64,
}

impl HumanReviewer for LocalOwnerReviewer {
    fn review(&self, _packet: &super::admission::ReviewPacket) -> ReviewDecision {
        ReviewDecision::Approve(AuthenticatedHumanDecision::local_owner(self.now))
    }
}

fn review_proposal(
    proposal_id: &str,
    approve: bool,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    let now = current_timestamp().context("failed to resolve review timestamp")?;
    if !approve {
        let mut store = SkillStore::open_at(paths)?;
        let admin = AdminIdentity::authenticated("local-owner")?;
        let skill_id =
            super::admission::reject_proposal(&mut store, Some(&admin), proposal_id, now)
                .context("learned-skill rejection failed")?;
        let status = store
            .revision_status(&skill_id)?
            .context("rejected revision disappeared")?;
        OperatorReport::new("reject")
            .with("id", skill_id)
            .with("status", status)
            .with("idempotent", false)
            .emit();
        return Ok(());
    }
    let mut evaluator = AdmissionEvaluator::new(
        SkillStore::open_at(paths)?,
        std::sync::Arc::new(Embedder::from_config(embedding)?),
        format!("local-review-{}", uuid::Uuid::new_v4()),
    )?;
    let outcome = evaluator
        .review_and_admit(proposal_id, &LocalOwnerReviewer { now }, now)
        .context("learned-skill review failed")?;
    match outcome {
        ReviewOutcome::Canary(result) => {
            // Admission advances the durable desired generation. The operator
            // command is not complete until that generation is published.
            drop(evaluator);
            let coordinator =
                IndexCoordinator::open(paths, Arc::new(Embedder::from_config(embedding)?))?;
            let generation = coordinator
                .rebuild_and_publish()
                .context("failed to publish approved learned-skill canary")?;
            // A replayed approval returns the *original* approval generation,
            // and the revision may have moved on since. Report what the store
            // holds now rather than restating the first decision.
            let status = live_revision_status(paths, &result.skill_id)?;
            approve_report(&result, generation, &status).emit();
        }
        ReviewOutcome::Denied | ReviewOutcome::Cancelled | ReviewOutcome::TimedOut => {
            anyhow::bail!("local-owner learned-skill review did not complete")
        }
    }
    Ok(())
}

/// Render an approval outcome.
///
/// `result.generation` is the generation of the *original* approval, which a
/// replayed approval returns unchanged, so the published generation and the
/// revision's live status are reported alongside it rather than instead of it.
fn approve_report(
    result: &super::store::CanaryApprovalResult,
    published_generation: u64,
    status: &str,
) -> OperatorReport {
    OperatorReport::new("approve")
        .with("id", result.skill_id.clone())
        .with("status", status.to_string())
        .with("generation", published_generation)
        .with("approval_generation", result.generation)
        .with("idempotent", result.idempotent)
}

/// The revision status the store holds right now, as the operator report's
/// single source of truth.
fn live_revision_status(paths: &AppPaths, skill_id: &str) -> anyhow::Result<String> {
    Ok(SkillStore::open_at(paths)?
        .revision_status(skill_id)?
        .unwrap_or_else(|| "absent".to_string()))
}

fn activate_skill(
    skill_id: &str,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    let now = current_timestamp().context("failed to resolve activation timestamp")?;
    // Validate the target *before* the index rebuild: a mistyped identifier
    // must not pay for a full embedding rebuild before being rejected.
    {
        let store = SkillStore::open_at(paths)?;
        let proposal = store
            .get_proposal(skill_id)?
            .context("learned-skill proposal not found")?;
        if proposal.predecessor_id.is_some() {
            anyhow::bail!(
                "learned skill {skill_id} is a replacement, not a lineage root; \
                 promote it with --promote-learned-skill instead of --activate-learned-skill"
            );
        }
        match store
            .revision_status(skill_id)?
            .as_deref()
            .context("learned-skill revision not found")?
        {
            "active" => {
                OperatorReport::new("activate")
                    .with("id", skill_id)
                    .with("status", "active")
                    .with("idempotent", true)
                    .emit();
                return Ok(());
            }
            "canary" => {}
            status => anyhow::bail!(
                "learned-skill activation requires an approved canary; current status is {status}"
            ),
        }
    }
    let embedder = Arc::new(Embedder::from_config(embedding)?);
    let coordinator = IndexCoordinator::open(paths, embedder)?;
    coordinator
        .rebuild_and_publish()
        .context("failed to reconcile learned-skill index before activation")?;
    let mut store = SkillStore::open_at(paths)?;
    let proposal = store
        .get_proposal(skill_id)?
        .context("learned-skill proposal not found")?;
    let report_id = proposal
        .report_id
        .context("learned-skill evaluation report is missing")?;
    let row_version = i64::try_from(
        store
            .revision_row_version(skill_id)?
            .context("learned-skill revision not found")?,
    )
    .context("learned-skill row version is out of range")?;
    let policy_version = "local-owner-eval-baseline-v1";
    LifecycleService::new(&mut store).register_policy(
        policy_version,
        r#"{"require_held_out_baseline":true,"require_second_local_owner_action":true}"#,
        now,
    )?;
    let approval = HumanApproval::local_owner(&report_id, row_version)?;
    let authorization =
        LifecycleService::new(&mut store).authorize_root_local_owner(skill_id, &approval, now)?;
    let generation = store.generation_state()?.desired_generation;
    drop(store);
    let snapshot = EvidenceSnapshot::new(
        skill_id,
        None,
        policy_version,
        // Root activation is authorized by the report-bound second owner
        // action above; it does not require a row in skill_evidence.
        Vec::new(),
        BTreeMap::new(),
        row_version,
        None,
        i64::try_from(generation).context("learned-skill generation is out of range")?,
    )?;
    let (outcome, publication) = CoordinatedLifecycle::new(&coordinator)
        .activate_root(
            &format!("local-owner-activate-{skill_id}"),
            skill_id,
            &approval,
            &authorization,
            &snapshot,
            now,
        )
        .context("learned-skill activation failed")?;
    OperatorReport::new("activate")
        .with("id", skill_id)
        .with("status", outcome.status.as_token())
        .with("generation", outcome.desired_generation)
        .with("removal_only", publication.removal_only)
        .with("idempotent", false)
        .emit();
    Ok(())
}

/// Policy row that records *why* an operator promotion was permitted. It is
/// deliberately not a `PromotionPolicy`: nothing in this path is decided by an
/// evidence threshold.
const OPERATOR_PROMOTION_POLICY_VERSION: &str = "local-owner-operator-promotion-v1";
const OPERATOR_PROMOTION_POLICY_JSON: &str = r#"{"operator_promotion":true,"require_second_local_owner_action":true,"evidence_threshold_promotion":false}"#;
const OPERATOR_PROMOTION_REASON: &str = "operator_local_owner_replacement_promotion";

/// Promote an approved replacement canary over its predecessor.
///
/// This is the explicit local-owner counterpart of [`activate_skill`]: it runs
/// through the same coordinated lifecycle façade, so the immutable index is
/// rebuilt and published exactly as activation does. The predecessor may be
/// `active` (an ordinary replacement) or `quarantined` (the emergency path for
/// a defective active skill); lineage is preserved rather than re-rooted.
fn promote_replacement_skill(
    skill_id: &str,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    let now = current_timestamp().context("failed to resolve promotion timestamp")?;
    let embedder = Arc::new(Embedder::from_config(embedding)?);
    let coordinator = IndexCoordinator::open(paths, embedder)?;
    coordinator
        .rebuild_and_publish()
        .context("failed to reconcile the learned-skill index before promotion")?;
    let mut store = SkillStore::open_at(paths)?;
    let proposal = store
        .get_proposal(skill_id)?
        .context("learned-skill proposal not found")?;
    let Some(predecessor_id) = proposal.predecessor_id.clone() else {
        anyhow::bail!(
            "learned skill {skill_id} is a lineage root with no predecessor; \
             activate it with --activate-learned-skill instead"
        );
    };
    let candidate = LifecycleService::new(&mut store).revision(skill_id)?;
    match candidate.status {
        LifecycleStatus::Active => {
            OperatorReport::new("promote")
                .with("id", skill_id)
                .with("status", "active")
                .with("predecessor", predecessor_id)
                .with("idempotent", true)
                .emit();
            return Ok(());
        }
        LifecycleStatus::Canary => {}
        status => anyhow::bail!(
            "learned-skill replacement promotion requires an approved canary; \
             revision {skill_id} is {status}"
        ),
    }
    if candidate.supersedes_id.as_deref() != Some(predecessor_id.as_str()) {
        anyhow::bail!(
            "learned-skill replacement promotion requires a canary bound to its predecessor; \
             revision {skill_id} supersedes {} but its proposal names {predecessor_id}",
            candidate.supersedes_id.as_deref().unwrap_or("nothing"),
        );
    }
    let predecessor = LifecycleService::new(&mut store).revision(&predecessor_id)?;
    if !matches!(
        predecessor.status,
        LifecycleStatus::Active | LifecycleStatus::Quarantined
    ) {
        anyhow::bail!(
            "learned-skill replacement promotion requires an active or quarantined predecessor; \
             predecessor {predecessor_id} is {}",
            predecessor.status
        );
    }
    let report_id = proposal
        .report_id
        .clone()
        .context("learned-skill evaluation report is missing")?;
    let generation = i64::try_from(store.generation_state()?.desired_generation)
        .context("learned-skill generation is out of range")?;

    // Binding the attempt to the observed row versions and generation keeps a
    // retry of the *same* observation an exact replay, while any later attempt
    // over moved rows gets its own key instead of colliding with this one.
    let attempt = format!(
        "{}-{}-{generation}",
        candidate.row_version, predecessor.row_version
    );
    let evidence_id = format!("operator-promotion:{skill_id}:{attempt}");
    let evidence_payload = serde_json::json!({
        "actor": "local-owner",
        "action": "explicit_operator_promotion",
        "candidate_id": skill_id,
        "predecessor_id": predecessor_id,
        "predecessor_from": predecessor.status.as_token(),
        "candidate_row_version": candidate.row_version,
        "predecessor_row_version": predecessor.row_version,
        "index_generation": generation,
        "evaluation_report_id": report_id,
        "evidence_threshold_promotion": false
    })
    .to_string();
    LifecycleService::new(&mut store).register_policy(
        OPERATOR_PROMOTION_POLICY_VERSION,
        OPERATOR_PROMOTION_POLICY_JSON,
        now,
    )?;
    LifecycleService::new(&mut store).record_operator_promotion_evidence(
        &evidence_id,
        skill_id,
        OPERATOR_PROMOTION_POLICY_VERSION,
        &evidence_payload,
        now,
    )?;
    let approval = HumanApproval::local_owner(&report_id, candidate.row_version)?;
    let authorization = LifecycleService::new(&mut store)
        .authorize_replacement_local_owner(skill_id, &approval, now)?;
    drop(store);

    let policy_inputs = BTreeMap::from([(
        "operator_promotion".to_string(),
        serde_json::json!({
            "actor": "local-owner",
            "predecessor_from": predecessor.status.as_token(),
            "evaluation_report_id": report_id
        }),
    )]);
    let snapshot = EvidenceSnapshot::new(
        skill_id,
        Some(predecessor_id.clone()),
        OPERATOR_PROMOTION_POLICY_VERSION,
        vec![evidence_id],
        policy_inputs,
        candidate.row_version,
        Some(predecessor.row_version),
        generation,
    )?;
    let request = ReplacementTransitionRequest {
        idempotency_key: format!("local-owner-promote:{skill_id}:{attempt}"),
        candidate_id: skill_id.to_string(),
        predecessor_id: predecessor_id.clone(),
        candidate_row_version: candidate.row_version,
        predecessor_row_version: predecessor.row_version,
        reason: OPERATOR_PROMOTION_REASON.to_string(),
        snapshot,
    };
    let (outcome, publication) = CoordinatedLifecycle::new(&coordinator)
        .promote_replacement_by_local_owner(
            &request,
            &approval,
            &authorization,
            predecessor.status,
            now,
        )
        .context("learned-skill replacement promotion failed")?;
    OperatorReport::new("promote")
        .with("id", skill_id)
        .with("status", outcome.candidate_status.as_token())
        .with("predecessor", predecessor_id)
        .with("predecessor_status", outcome.predecessor_status.as_token())
        .with("generation", outcome.desired_generation)
        .with("removal_only", publication.removal_only)
        .with("idempotent", false)
        .emit();
    Ok(())
}

fn proposal_status(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Evaluating => "evaluating",
        ProposalStatus::Deferred => "deferred",
        ProposalStatus::Verified => "verified",
        ProposalStatus::Rejected => "rejected",
        ProposalStatus::AwaitingApproval => "awaiting_approval",
        ProposalStatus::Approved => "approved",
    }
}

fn submit_feedback(
    operation: FeedbackOperation<'_>,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    let kind = match operation.kind {
        "positive" => FeedbackKind::Positive,
        "negative" => FeedbackKind::Negative,
        "severe" => FeedbackKind::Severe,
        _ => anyhow::bail!("invalid learned-skill feedback kind"),
    };
    let now = current_timestamp().context("failed to resolve feedback timestamp")?;
    let mut store = SkillStore::open_at(paths).context("failed to open learned-skill store")?;
    let command = FeedbackCommand {
        idempotency_key: operation.idempotency_key.to_string(),
        skill_id: operation.skill_id.to_string(),
        invocation_id: operation.invocation_id.map(str::to_string),
        kind,
        reason_code: operation.reason_code.to_string(),
        reason_text: None,
    };
    let actor = AuthenticatedActor {
        actor_id: "local-owner".to_string(),
        kind: ActorKind::Owner,
        allowed_skill_ids: Some([operation.skill_id.to_string()].into_iter().collect()),
    };
    let feedback_id = FeedbackService::new(&mut store, Redactor::new(Vec::new(), 512))
        .submit(&actor, &command, now)
        .context("learned-skill feedback submission failed")?;

    // Severe feedback is containment only where the target is retrievable or
    // about to be. Reporting the same "recorded" line for every status hid
    // whether anything was actually quarantined.
    let (quarantine, quarantine_detail) = if kind == FeedbackKind::Severe {
        let attribution = FeedbackAttribution::new(feedback_id.as_str(), operation.reason_code);
        contain_severe_feedback(store, &operation, &attribution, paths, embedding, now)?
    } else {
        drop(store);
        ("not_applicable", None)
    };
    // Read the status back after the action, so an applied quarantine shows up
    // in the same line that records the feedback.
    let status = live_revision_status(paths, operation.skill_id)?;
    OperatorReport::new("feedback")
        .with("id", operation.skill_id)
        .with("feedback_id", feedback_id)
        .with("kind", operation.kind)
        .with("status", status)
        .with("quarantine", quarantine)
        .with("quarantine_reason", quarantine_detail)
        .emit();
    Ok(())
}

/// Quarantine the target of severe feedback when its lifecycle status allows
/// it, reporting `applied`, `held` (with the policy's hold reason) or
/// `skipped` (with the ineligible status) rather than staying silent.
fn contain_severe_feedback(
    store: SkillStore,
    operation: &FeedbackOperation<'_>,
    attribution: &FeedbackAttribution,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
    now: i64,
) -> anyhow::Result<(&'static str, Option<String>)> {
    let metadata = store
        .metadata(operation.skill_id)
        .context("failed to inspect feedback target")?
        .context("feedback target disappeared")?;
    let status = LifecycleStatus::from_token(&metadata.status)
        .context("feedback target has an invalid lifecycle status")?;
    if status != LifecycleStatus::Canary && status != LifecycleStatus::Active {
        return Ok((
            "skipped",
            Some(format!("ineligible_status:{}", status.as_token())),
        ));
    }
    drop(store);
    let embedder = Arc::new(Embedder::from_config(embedding)?);
    let coordinator = IndexCoordinator::open(paths, embedder)?;
    coordinator
        .rebuild_and_publish()
        .context("failed to reconcile the learned-skill index before quarantine")?;
    let store = SkillStore::open_at(paths)?;
    let generation = store.generation_state()?;
    let metadata = store
        .metadata(operation.skill_id)?
        .context("feedback target disappeared before quarantine")?;
    let reason = if status == LifecycleStatus::Canary {
        QuarantineReason::AuthenticatedCanarySafetyFeedback
    } else {
        QuarantineReason::AuthenticatedActiveIntegrityFeedback
    };
    let evidence = QuarantineEvidence {
        skill_id: operation.skill_id.to_string(),
        reason,
        qualified_invocations: 0,
        direct_failures: 0,
        evidence_complete: true,
        authenticated_feedback: true,
        feedback_marked_severe: true,
        row_version_current: true,
        generation_current: generation.desired_generation == generation.applied_generation,
    };
    let row_version = i64::try_from(metadata.row_version)
        .context("feedback target row version is out of range")?;
    let desired_generation = i64::try_from(generation.desired_generation)
        .context("feedback target generation is out of range")?;
    drop(store);
    // The attribution carries the submitted reason code and the stored feedback
    // row into the evidence snapshot and the transition reason, so an operator
    // `permission_violation` is not flattened into the lifecycle-derived
    // quarantine reason.
    match QuarantineExecutor::new(&coordinator).apply_with_attribution(
        &QuarantinePolicy::conservative("phase5-quarantine-v1"),
        &evidence,
        Some(attribution),
        status,
        row_version,
        desired_generation,
        now,
    ) {
        Ok(_) => Ok(("applied", None)),
        // A policy hold is a decision, not an infrastructure fault: the
        // feedback stays recorded and the operator is told why nothing was
        // contained.
        Err(QuarantineExecutionError::Held(held)) => Ok(("held", Some(held.to_string()))),
        Err(error) => {
            Err(anyhow::Error::new(error)
                .context("severe feedback was stored but quarantine failed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::telemetry::{
        EventBatch, SkillEvent, SkillEventKind, TelemetryIngestor, stable_invocation_id,
    };
    use super::*;
    use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
    use crate::paths::{PathEnvironment, PathPlatform};

    thread_local! {
        pub(super) static BEFORE_PACKAGE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
            std::cell::RefCell::new(None)
        };
    }

    fn unavailable_embedding_config() -> EmbeddingConfig {
        let key = format!(
            "MINI_AGENT_MISSING_EMBEDDING_{}",
            uuid::Uuid::new_v4().simple()
        );
        assert!(std::env::var_os(&key).is_none());
        let config = EmbeddingConfig {
            backend: crate::config::EmbeddingBackendKind::External,
            api_key_env: Some(key.into()),
            ..EmbeddingConfig::default()
        };
        assert!(
            Embedder::from_config(Some(&config)).is_err(),
            "fixture must prevent embedding initialization"
        );
        config
    }

    fn assert_rejection_preserves_approved_state(paths: &AppPaths, skill_id: &str) {
        let store = SkillStore::open_at(paths).unwrap();
        let proposal = store.get_proposal(skill_id).unwrap().unwrap();
        let revision = store.revision_status(skill_id).unwrap();
        let generation = store.desired_generation().unwrap();
        drop(store);
        assert!(
            review_proposal(skill_id, false, paths, None).is_err(),
            "rejection must not replay approval for an approved proposal"
        );
        let store = SkillStore::open_at(paths).unwrap();
        assert_eq!(store.get_proposal(skill_id).unwrap().unwrap(), proposal);
        assert_eq!(store.revision_status(skill_id).unwrap(), revision);
        assert_eq!(store.desired_generation().unwrap(), generation);
    }

    fn fixture() -> (std::path::PathBuf, AppPaths, SkillArtifact) {
        let root = std::env::temp_dir().join(format!(
            "skill-operations-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let paths = AppPaths::resolve(&PathEnvironment {
            platform: if cfg!(target_os = "macos") {
                PathPlatform::MacOs
            } else if cfg!(target_os = "windows") {
                PathPlatform::Windows
            } else {
                PathPlatform::Linux
            },
            home_dir: None,
            config_base: Some(root.join("config")),
            data_base: Some(root.join("data")),
            local_data_base: Some(root.join("local")),
            state_base: Some(root.join("state")),
            cache_base: Some(root.join("cache")),
            workspace_root: None,
            overrides: Default::default(),
        })
        .unwrap();
        let artifact = SkillArtifact::new(
            "function run() { return 1; }".into(),
            "Operator surface fixture".into(),
            vec![],
            vec![SkillExport {
                name: "run".into(),
                signature: "() => number".into(),
            }],
            vec!["run() === 1".into()],
            CapabilityManifest::pure(),
        )
        .unwrap();
        (root, paths, artifact)
    }

    #[test]
    fn explicit_purge_removes_dependent_bytes_and_acknowledges_publication() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        drop(store);

        run(
            Some(PurgeOperation {
                skill_id: &artifact.id,
                force: true,
            }),
            false,
            None,
            None,
            &paths,
            None,
        )
        .unwrap();

        let store = SkillStore::open_at(&paths).unwrap();
        assert!(store.get(&artifact.id).unwrap().is_none());
        let state = store.generation_state().unwrap();
        assert_eq!(state.desired_generation, state.applied_generation);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_compaction_is_safe_on_an_empty_store() {
        let (root, paths, _artifact) = fixture();
        run(None, true, None, None, &paths, None).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn usage_stats_report_success_rate_last_use_and_conservative_savings() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        store
            .conn_mut()
            .execute(
                "INSERT INTO skill_stats (
                    skill_id, invoked_count, direct_success_count, direct_failure_count, updated_at
                 ) VALUES (?, 8, 6, 2, 42)",
                [&artifact.id],
            )
            .unwrap();
        store
            .conn_mut()
            .execute(
                "INSERT INTO skill_events (
                    invocation_id, skill_id, turn_id, event_kind, index_generation,
                    evidence_complete, production, created_at
                 ) VALUES (?, ?, 'turn-stats', 'invoked', 0, 1, 1, 42)",
                rusqlite::params!["a".repeat(64), artifact.id],
            )
            .unwrap();
        // Gym rows share the verify command with the production rows, so a
        // missing `production` predicate would fold them into the utility
        // columns instead of the gym column.
        for (evidence_id, turn_id, passed, linked, production) in [
            ("with-pass", "with-1", 1, true, 1),
            ("with-fail", "with-2", 0, true, 1),
            ("without-pass", "without-1", 1, false, 1),
            ("without-fail", "without-2", 0, false, 1),
            ("gym-with-pass", "gym-1", 1, true, 0),
            ("gym-with-fail", "gym-2", 0, true, 0),
            ("gym-without-pass", "gym-3", 1, false, 0),
        ] {
            store
                .conn_mut()
                .execute(
                    "INSERT INTO skill_task_outcomes (
                         evidence_id, turn_id, verify_passed, attempt, source_kind,
                         source_id, production, created_at
                     ) VALUES (?, ?, ?, 1, 'verify_command', 'same-command', ?, 42)",
                    rusqlite::params![evidence_id, turn_id, passed, production],
                )
                .unwrap();
            if linked {
                store
                    .conn_mut()
                    .execute(
                        "INSERT INTO skill_task_outcome_links (evidence_id, skill_id)
                         VALUES (?, ?)",
                        rusqlite::params![evidence_id, artifact.id],
                    )
                    .unwrap();
            }
        }

        let rows = load_skill_stats(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].invocations, 8);
        assert_eq!(rows[0].last_used, Some(42));
        assert_eq!(rows[0].success_percent(), Some(75.0));
        assert_eq!(rows[0].tasks_with, 2, "gym tasks must not count as utility");
        assert_eq!(rows[0].passed_with, 1);
        assert_eq!(rows[0].baseline_tasks, 2);
        assert_eq!(rows[0].pass_rate_without_percent(), Some(50.0));
        assert_eq!(rows[0].gym_tasks, 2, "gym tasks are reported separately");
        assert_eq!(rows[0].estimated_round_trips_saved, 0);
        let line = rows[0].to_line();
        assert!(
            line.contains("\t75.0%\t42\t2\t1\t50.0%\t2\t0\t0\t"),
            "{line}"
        );

        assert_eq!(estimate_round_trips_saved(6, 3), 12);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Build `revisions` retained skills, each with one observed production
    /// task and one no-library baseline under a shared oracle, and return how
    /// long `load_skill_stats` takes over that corpus.
    fn stats_latency_for(
        revisions: usize,
        shared_scope: bool,
    ) -> (std::time::Duration, Vec<SkillUsageStats>) {
        let (root, paths, _) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        let mut ids = Vec::with_capacity(revisions);
        for index in 0..revisions {
            let artifact = SkillArtifact::new(
                format!("function run() {{ return {index}; }}"),
                format!("Scaling fixture {index}"),
                vec![],
                vec![SkillExport {
                    name: "run".into(),
                    signature: "() => number".into(),
                }],
                vec![format!("run() === {index}")],
                CapabilityManifest::pure(),
            )
            .unwrap();
            store.insert_verified(&artifact).unwrap();
            ids.push(artifact.id.clone());
        }
        for (index, id) in ids.iter().enumerate() {
            // One observed task attributed to this revision, and one baseline
            // turn with no link at all, under the same oracle scope. A shared
            // scope makes every baseline comparable to every skill; distinct
            // scopes are the ordinary case, where each skill has one.
            let scope = if shared_scope {
                "shared-oracle".to_string()
            } else {
                format!("oracle-{index}")
            };
            store
                .conn_mut()
                .execute(
                    "INSERT INTO skill_task_outcomes (
                         evidence_id, turn_id, verify_passed, attempt, source_kind,
                         source_id, production, created_at
                     ) VALUES (?, ?, 1, 1, 'oracle', ?, 1, 42)",
                    rusqlite::params![format!("observed-{index}"), format!("turn-{index}"), scope],
                )
                .unwrap();
            store
                .conn_mut()
                .execute(
                    "INSERT INTO skill_task_outcome_links (evidence_id, skill_id)
                     VALUES (?, ?)",
                    rusqlite::params![format!("observed-{index}"), id],
                )
                .unwrap();
            store
                .conn_mut()
                .execute(
                    "INSERT INTO skill_task_outcomes (
                         evidence_id, turn_id, verify_passed, attempt, source_kind,
                         source_id, production, created_at
                     ) VALUES (?, ?, 0, 1, 'oracle', ?, 1, 42)",
                    rusqlite::params![
                        format!("baseline-{index}"),
                        format!("baseline-turn-{index}"),
                        scope
                    ],
                )
                .unwrap();
        }

        // Warm the page cache, then take the best of three runs so an unlucky
        // scheduling slice cannot decide the outcome.
        let rows = load_skill_stats(&store).unwrap();
        let mut best = std::time::Duration::MAX;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let _ = load_skill_stats(&store).unwrap();
            best = best.min(started.elapsed());
        }
        drop(store);
        let _ = std::fs::remove_dir_all(root);
        (best, rows)
    }

    /// The regression this pins is structural, not temporal.
    ///
    /// The previous shape ran a correlated subquery per retained revision that
    /// *scanned* every task outcome, with a further source-scope search inside
    /// it; doubling the corpus multiplied query work roughly eightfold. Wall
    /// clock cannot express that on a shared runner, so the plan is asserted
    /// instead: no correlated subquery may scan a table. A correlated subquery
    /// that probes an index (the links anti-join) stays allowed, because it is
    /// O(log n) per row rather than a rescan.
    #[test]
    fn the_statistics_plan_has_no_scanning_correlated_subquery() {
        let (root, paths, _) = fixture();
        let store = SkillStore::open_at(&paths).unwrap();
        let mut statement = store
            .connection()
            .prepare(&format!("EXPLAIN QUERY PLAN {SKILL_STATS_SQL}"))
            .unwrap();
        // (id, parent, detail) describes the plan tree; children of a node are
        // the rows whose parent is that node's id.
        let nodes: Vec<(i64, i64, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(3)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();

        let mut offenders = Vec::new();
        for (id, _, detail) in &nodes {
            if !detail.contains("CORRELATED") {
                continue;
            }
            for (_, parent, child) in &nodes {
                if parent == id && child.starts_with("SCAN") {
                    offenders.push(format!("{detail} -> {child}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a correlated subquery rescans a table for every row: {offenders:#?}\nplan: {:#?}",
            nodes.iter().map(|node| &node.2).collect::<Vec<_>>()
        );
        drop(statement);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn usage_stats_do_not_rescan_every_baseline_for_every_revision() {
        let (_, rows) = stats_latency_for(80, false);

        // With one oracle scope per skill, each revision's comparison group is
        // its own baseline turn.
        assert_eq!(rows.len(), 80);
        for row in &rows {
            assert_eq!(row.tasks_with, 1);
            assert_eq!(row.passed_with, 1);
            assert_eq!(row.baseline_tasks, 1);
            assert_eq!(row.baseline_passes, 0);
            assert_eq!(row.gym_tasks, 0);
        }
    }

    #[test]
    fn usage_stats_compare_against_every_baseline_sharing_a_scope() {
        let (_, rows) = stats_latency_for(40, true);
        assert_eq!(rows.len(), 40);
        for row in &rows {
            assert_eq!(row.tasks_with, 1);
            assert_eq!(
                row.baseline_tasks, 40,
                "every baseline under the shared oracle is comparable"
            );
            assert_eq!(row.baseline_passes, 0);
        }
    }

    #[test]
    fn usage_stats_keep_scopes_and_attempts_separate() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();

        // Two source scopes: only baselines under a scope this skill was
        // actually observed in may count as its comparison group.
        for (evidence_id, turn_id, source_id, linked, passed) in [
            ("observed-a", "turn-a", "oracle-a", true, 1),
            ("baseline-a", "turn-b", "oracle-a", false, 1),
            ("baseline-b", "turn-c", "oracle-b", false, 0),
        ] {
            store
                .conn_mut()
                .execute(
                    "INSERT INTO skill_task_outcomes (
                         evidence_id, turn_id, verify_passed, attempt, source_kind,
                         source_id, production, created_at
                     ) VALUES (?, ?, ?, 1, 'oracle', ?, 1, 42)",
                    rusqlite::params![evidence_id, turn_id, passed, source_id],
                )
                .unwrap();
            if linked {
                store
                    .conn_mut()
                    .execute(
                        "INSERT INTO skill_task_outcome_links (evidence_id, skill_id)
                         VALUES (?, ?)",
                        rusqlite::params![evidence_id, artifact.id],
                    )
                    .unwrap();
            }
        }
        // A second attempt on the same turn must not double-count it.
        store
            .conn_mut()
            .execute(
                "INSERT INTO skill_task_outcomes (
                     evidence_id, turn_id, verify_passed, attempt, source_kind,
                     source_id, production, created_at
                 ) VALUES ('baseline-a2', 'turn-b', 0, 2, 'oracle', 'oracle-a', 1, 43)",
                rusqlite::params![],
            )
            .unwrap();

        let rows = load_skill_stats(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tasks_with, 1);
        assert_eq!(
            rows[0].baseline_tasks, 1,
            "only the observed scope's baseline turn counts, once"
        );
        assert_eq!(rows[0].baseline_passes, 1);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn usage_stats_exclude_turns_whose_evidence_was_lost() {
        use super::super::policy::{TaskOutcomeEvidence, TaskOutcomeSource};
        use super::super::telemetry::TelemetryDispatcher;

        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        // The queue accepts these valid events, but SQLite rejects their
        // transaction later. The caller cannot see that asynchronous failure.
        store
            .conn()
            .execute_batch(
                "CREATE TRIGGER reject_selected_evidence BEFORE INSERT ON skill_events
             WHEN NEW.turn_id IN ('worker-lost', 'late-loss')
              AND NEW.event_kind = 'selected'
             BEGIN SELECT RAISE(ABORT, 'injected ingestion failure'); END;
             CREATE TRIGGER reject_task_evidence BEFORE INSERT ON skill_task_outcomes
             WHEN NEW.turn_id = 'task-lost' AND NEW.attempt = 2
             BEGIN SELECT RAISE(ABORT, 'injected task ingestion failure'); END;",
            )
            .unwrap();
        let dispatcher = TelemetryDispatcher::spawn(&paths).unwrap();
        let event = |turn: &str, kind| SkillEvent {
            invocation_id: Some(stable_invocation_id(turn, "tool", &artifact.id, "run", 0)),
            skill_id: artifact.id.clone(),
            turn_id: turn.into(),
            tool_call_id: Some("tool".into()),
            kind,
            export_name: Some("run".into()),
            outcome: None,
            latency_us: None,
            retrieval_score: None,
            retrieval_rank: None,
            query_fingerprint: None,
            index_generation: 0,
            evidence_complete: true,
            production: true,
            argument_shape: None,
            created_at: 2_000_000_000,
        };
        let outcome = |turn: &str, evidence_complete, attempt| {
            dispatcher
                .record_task_outcome(TaskOutcomeEvidence {
                    turn_id: turn.into(),
                    skill_ids: vec![artifact.id.clone()],
                    verify_passed: true,
                    attempt,
                    source: TaskOutcomeSource::Oracle("shared-oracle".into()),
                    production: true,
                    evidence_complete,
                    created_at: 2_000_000_001,
                })
                .unwrap();
        };
        for turn in ["healthy-observed", "late-loss"] {
            dispatcher
                .try_dispatch(EventBatch::new(vec![event(turn, SkillEventKind::Invoked)]).unwrap())
                .unwrap();
            outcome(turn, true, 1);
        }
        // A later failed write must also invalidate the already stored pass.
        dispatcher
            .try_dispatch(
                EventBatch::new(vec![event("late-loss", SkillEventKind::Selected)]).unwrap(),
            )
            .unwrap();
        // Both events roll back, so there is no invocation link left to keep
        // this turn out of the no-library baseline by itself.
        dispatcher
            .try_dispatch(
                EventBatch::new(vec![
                    event("worker-lost", SkillEventKind::Invoked),
                    event("worker-lost", SkillEventKind::Selected),
                ])
                .unwrap(),
            )
            .unwrap();
        outcome("worker-lost", true, 1);
        outcome("worker-lost", true, 1); // replay cannot restore completeness
        let mut incomplete = event("worker-incomplete", SkillEventKind::Invoked);
        incomplete.evidence_complete = false;
        dispatcher
            .try_dispatch(EventBatch::new(vec![incomplete]).unwrap())
            .unwrap();
        outcome("worker-incomplete", true, 1);
        outcome("parent-lost", true, 1);
        outcome("parent-lost", false, 1);
        outcome("parent-lost", true, 1); // a late snapshot cannot heal a lost turn
        outcome("healthy-baseline", true, 1); // unrelated later turns still qualify
        outcome("task-lost", true, 1);
        outcome("task-lost", true, 2); // fails after a pass was stored
        outcome("task-lost", true, 1); // retry cannot heal the worker-side loss
        let probe = dispatcher.shutdown_probe_for_test();
        drop(dispatcher); // flush the FIFO before inspecting durable evidence
        assert_eq!(probe().1, 3);
        let mut statement = store
            .conn()
            .prepare("SELECT turn_id, evidence_complete FROM skill_task_outcomes ORDER BY turn_id")
            .unwrap();
        let completeness = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            completeness,
            vec![
                ("healthy-baseline".into(), true),
                ("healthy-observed".into(), true),
                ("late-loss".into(), false),
                ("parent-lost".into(), false),
                ("task-lost".into(), false),
                ("worker-incomplete".into(), false),
                ("worker-lost".into(), false),
            ]
        );
        let restarted = TelemetryDispatcher::spawn(&paths).unwrap();
        restarted
            .record_task_outcome(TaskOutcomeEvidence {
                turn_id: "worker-lost".into(),
                skill_ids: vec![artifact.id.clone()],
                verify_passed: true,
                attempt: 2,
                source: TaskOutcomeSource::Oracle("shared-oracle".into()),
                production: true,
                evidence_complete: true,
                created_at: 2_000_000_002,
            })
            .unwrap();
        drop(restarted);
        let complete: bool = store.conn().query_row(
            "SELECT evidence_complete FROM skill_task_outcomes WHERE turn_id = 'worker-lost' AND attempt = 2",
            [], |row| row.get(0),
        ).unwrap();
        assert!(!complete, "a restarted dispatcher cannot heal a lost turn");
        let rows = load_skill_stats(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].tasks_with, rows[0].passed_with), (1, 1));
        assert_eq!((rows[0].baseline_tasks, rows[0].baseline_passes), (1, 1));
        drop(statement);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn usage_stats_exclude_turns_with_a_recorded_observability_loss() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();

        for (evidence_id, turn_id) in [
            ("healthy-baseline", "turn-baseline"),
            ("lost-baseline", "turn-lost"),
        ] {
            store
                .conn_mut()
                .execute(
                    "INSERT INTO skill_task_outcomes (
                         evidence_id, turn_id, verify_passed, attempt, source_kind,
                         source_id, production, evidence_complete, created_at
                     ) VALUES (?, ?, 1, 1, 'oracle', 'shared-oracle', 1, 1, 42)",
                    rusqlite::params![evidence_id, turn_id],
                )
                .unwrap();
        }
        // The observed task keeps the skill's scope comparable to the
        // baselines above.
        store
            .conn_mut()
            .execute(
                "INSERT INTO skill_task_outcomes (
                     evidence_id, turn_id, verify_passed, attempt, source_kind,
                     source_id, production, evidence_complete, created_at
                 ) VALUES ('observed', 'turn-observed', 1, 1, 'oracle', 'shared-oracle', 1, 1, 42)",
                rusqlite::params![],
            )
            .unwrap();
        store
            .conn_mut()
            .execute(
                "INSERT INTO skill_task_outcome_links (evidence_id, skill_id)
                 VALUES ('observed', ?)",
                rusqlite::params![artifact.id],
            )
            .unwrap();
        // An explicit loss event for one baseline turn.
        store
            .conn_mut()
            .execute(
                "INSERT INTO skill_events (
                    invocation_id, skill_id, turn_id, event_kind, index_generation,
                    evidence_complete, production, created_at
                 ) VALUES (?, ?, 'turn-lost', 'observability_lost', 0, 0, 1, 42)",
                rusqlite::params!["b".repeat(64), artifact.id],
            )
            .unwrap();

        let rows = load_skill_stats(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].baseline_tasks, 1,
            "a turn with a recorded observability loss must not be a baseline"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn usage_stats_distinguish_an_absent_baseline_from_a_failing_one() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        drop(store);

        let store = SkillStore::open_at(&paths).unwrap();
        let rows = load_skill_stats(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].baseline_tasks, 0);
        assert_eq!(
            rows[0].pass_rate_without_percent(),
            None,
            "no baseline task must not read as a 0% baseline"
        );
        assert_eq!(rows[0].success_percent(), None);
        let line = rows[0].to_line();
        assert_eq!(
            line.split('\t').filter(|field| *field == "n/a").count(),
            2,
            "{line}"
        );
        assert_eq!(
            line.split('\t').count(),
            SKILL_STATS_HEADER.split('\t').count(),
            "every column needs a header"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_severe_feedback_is_persisted_and_quarantines_active_skill() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        drop(store);

        run(
            None,
            false,
            Some(FeedbackOperation {
                skill_id: &artifact.id,
                invocation_id: None,
                kind: "severe",
                reason_code: "integrity",
                idempotency_key: "operator-feedback-1",
            }),
            None,
            &paths,
            None,
        )
        .unwrap();

        let store = SkillStore::open_at(&paths).unwrap();
        assert_eq!(
            store.metadata(&artifact.id).unwrap().unwrap().status,
            "quarantined"
        );
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM skill_feedback", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_seeds_are_canonical_pure_packages_with_held_out_baselines() {
        for (name, source) in SEED_PACKAGES {
            assert!(source.len() as u64 <= MAX_PACKAGE_BYTES, "{name}");
            let package: LearnedSkillPackage = serde_json::from_str(source).unwrap();
            assert!(!package.held_out_suites.is_empty(), "{name}");
            let artifact = JsProposal::try_from(package.proposal)
                .unwrap()
                .validate_and_canonicalize()
                .unwrap();
            assert_eq!(artifact.capability, CapabilityManifest::pure(), "{name}");
            artifact.verify_identity().unwrap();
        }
    }

    /// Drive the whole operator funnel once and assert every invariant the
    /// lifecycle promises, at the step that establishes it.
    ///
    /// import -> approve -> activate -> telemetry -> refused duplicate ->
    /// replacement -> canary routing -> containment -> promotion ->
    /// withdrawal -> purge.
    #[test]
    fn operator_episode_drives_the_whole_funnel_and_holds_its_invariants() {
        let (root, paths, _) = fixture();
        std::fs::create_dir_all(&root).unwrap();
        let package_path = root.join("episode-seed.json");
        std::fs::write(&package_path, SEED_PACKAGES[0].1).unwrap();
        let package: LearnedSkillPackage = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        let artifact = JsProposal::try_from(package.proposal)
            .unwrap()
            .validate_and_canonicalize()
            .unwrap();
        let root_id = artifact.id.clone();
        let export_name = artifact.exports[0].name.clone();

        // Every step re-reads the durable store and re-verifies the bytes it
        // hands back. Verifying the in-memory artifact the test already holds
        // would assert nothing about what any step persisted.
        let assert_state = |expected: &str| {
            let store = SkillStore::open_at(&paths).unwrap();
            assert_eq!(store.metadata(&root_id).unwrap().unwrap().status, expected);
            let generation = store.generation_state().unwrap();
            assert_eq!(
                generation.desired_generation, generation.applied_generation,
                "published index must match durable desired generation after {expected}"
            );
            let stored = store
                .get(&root_id)
                .unwrap()
                .expect("the immutable revision must survive every lifecycle step");
            stored
                .verify_identity()
                .expect("stored bytes must still hash to the stored identity");
            assert_eq!(stored.source, artifact.source);
        };

        run(
            None,
            false,
            None,
            Some(LibraryOperation::Import(&package_path)),
            &paths,
            None,
        )
        .unwrap();
        assert_state("verified");

        run(
            None,
            false,
            None,
            Some(LibraryOperation::Approve(&root_id)),
            &paths,
            None,
        )
        .unwrap();
        assert_state("canary");

        run(
            None,
            false,
            None,
            Some(LibraryOperation::Activate(&root_id)),
            &paths,
            None,
        )
        .unwrap();
        assert_state("active");

        // Telemetry ingestion: one production invocation must reach the
        // operator-visible usage surface, and an invocation with no terminal
        // event must not be reported as a 0% success rate.
        let mut store = SkillStore::open_at(&paths).unwrap();
        let index_generation = store.generation_state().unwrap().applied_generation;
        TelemetryIngestor::new(&mut store)
            .ingest(
                &EventBatch::new(vec![SkillEvent {
                    invocation_id: Some(stable_invocation_id(
                        "episode-turn",
                        "episode-tool",
                        &root_id,
                        &export_name,
                        0,
                    )),
                    skill_id: root_id.clone(),
                    turn_id: "episode-turn".to_string(),
                    tool_call_id: Some("episode-tool".to_string()),
                    kind: SkillEventKind::Invoked,
                    export_name: Some(export_name.clone()),
                    outcome: None,
                    latency_us: Some(1_500),
                    retrieval_score: Some(0.9),
                    retrieval_rank: Some(1),
                    query_fingerprint: None,
                    index_generation,
                    evidence_complete: true,
                    production: true,
                    argument_shape: None,
                    created_at: 4_242,
                }])
                .unwrap(),
            )
            .unwrap();
        let ingested = load_skill_stats(&store)
            .unwrap()
            .into_iter()
            .find(|row| row.skill_id == root_id)
            .expect("the active revision must appear in the usage report");
        assert_eq!(ingested.invocations, 1);
        assert_eq!(ingested.last_used, Some(4_242));
        assert_eq!(ingested.gym_tasks, 0);
        assert_eq!(
            ingested.success_percent(),
            None,
            "an invocation with no terminal event has no success rate yet"
        );
        drop(store);

        // A same-contract sibling in another lineage is refused by admission,
        // and the refusal is terminal: it must not keep spending the attempt
        // budget afterwards.
        let (duplicate_json, duplicate_id) = duplicate_package_json("episode-duplicate");
        let duplicate_path = root.join("episode-duplicate.json");
        std::fs::write(&duplicate_path, duplicate_json).unwrap();
        let refusal = run(
            None,
            false,
            None,
            Some(LibraryOperation::Import(&duplicate_path)),
            &paths,
            None,
        )
        .unwrap_err();
        let refusal = format!("{refusal:#}");
        assert!(refusal.contains("duplicate_skill"), "{refusal}");
        let store = SkillStore::open_at(&paths).unwrap();
        let refused = store.get_proposal(&duplicate_id).unwrap().unwrap();
        assert_eq!(refused.status, ProposalStatus::Rejected);
        assert!(
            (1..=MAX_EVALUATION_ATTEMPTS).contains(&refused.attempt_count),
            "attempt budget exceeded: {}",
            refused.attempt_count
        );
        assert_eq!(
            due_proposal_head(&store, current_timestamp().unwrap()).unwrap(),
            None,
            "a terminally refused proposal must stop consuming the attempt budget"
        );
        drop(store);

        // An approved replacement is a routable canary over its active
        // predecessor, on the exact applied generation.
        let replacement_id = approved_replacement(&paths, "episode-replacement", &root_id);
        assert_eq!(
            revision_state(&paths, &replacement_id).status,
            LifecycleStatus::Canary
        );
        let root_lineage = revision_state(&paths, &root_id).lineage_root_id;
        assert_eq!(
            root_lineage, root_id,
            "an activated lineage root is its own lineage root"
        );
        let coordinator =
            IndexCoordinator::open(&paths, Arc::new(Embedder::from_config(None).unwrap())).unwrap();
        coordinator.rebuild_and_publish().unwrap();
        let generation = SkillStore::open_at(&paths)
            .unwrap()
            .generation_state()
            .unwrap()
            .applied_generation;
        let routing = coordinator
            .routing_context(std::slice::from_ref(&root_id), generation)
            .unwrap()
            .expect("routing must resolve against the applied generation");
        let (routed_artifact, canary) = routing
            .candidates
            .get(&root_id)
            .expect("an approved canary must be routable over its active predecessor");
        assert_eq!(canary.candidate_id, replacement_id);
        assert_eq!(canary.status, LifecycleStatus::Canary);
        assert_eq!(canary.lineage_root_id, root_lineage);
        assert!(canary.identity_valid);
        assert_eq!(routed_artifact.id, replacement_id);
        drop(coordinator);

        // Severe feedback contains the active predecessor without stranding
        // its replacement.
        run(
            None,
            false,
            Some(FeedbackOperation {
                skill_id: &root_id,
                invocation_id: None,
                kind: "severe",
                reason_code: "integrity",
                idempotency_key: "episode-severe",
            }),
            None,
            &paths,
            None,
        )
        .unwrap();
        assert_state("quarantined");

        // Promotion keeps lineage in both directions.
        run(
            None,
            false,
            None,
            Some(LibraryOperation::Promote(&replacement_id)),
            &paths,
            None,
        )
        .unwrap();
        let promoted = revision_state(&paths, &replacement_id);
        let superseded = revision_state(&paths, &root_id);
        assert_eq!(promoted.status, LifecycleStatus::Active);
        assert_eq!(superseded.status, LifecycleStatus::Superseded);
        assert_eq!(promoted.supersedes_id.as_deref(), Some(&*root_id));
        assert_eq!(promoted.lineage_root_id, superseded.lineage_root_id);
        assert_eq!(
            superseded.superseded_by_id.as_deref(),
            Some(&*replacement_id)
        );

        // Withdrawal clears retrieval and routing, and nothing else.
        run(
            None,
            false,
            None,
            Some(LibraryOperation::Retire(&replacement_id)),
            &paths,
            None,
        )
        .unwrap();
        let withdrawn = revision_state(&paths, &replacement_id);
        assert_eq!(withdrawn.status, LifecycleStatus::Retired);
        assert_eq!(
            withdrawn.supersedes_id.as_deref(),
            Some(&*root_id),
            "withdrawal must not erase lineage"
        );
        let store = SkillStore::open_at(&paths).unwrap();
        assert!(
            !store.is_retrievable(&replacement_id).unwrap(),
            "a withdrawn skill must leave the retrieval set"
        );
        assert!(
            store.get(&replacement_id).unwrap().is_some(),
            "withdrawal keeps the immutable bytes; only purge removes them"
        );
        let generation = store.generation_state().unwrap();
        assert_eq!(generation.desired_generation, generation.applied_generation);
        drop(store);
        let coordinator =
            IndexCoordinator::open(&paths, Arc::new(Embedder::from_config(None).unwrap())).unwrap();
        let generation = coordinator.rebuild_and_publish().unwrap();
        assert!(
            coordinator
                .routing_context(std::slice::from_ref(&replacement_id), generation)
                .unwrap()
                .expect("routing must resolve against the applied generation")
                .candidates
                .is_empty(),
            "nothing may route into a withdrawn lineage"
        );
        drop(coordinator);

        run(
            Some(PurgeOperation {
                skill_id: &replacement_id,
                force: true,
            }),
            false,
            None,
            None,
            &paths,
            None,
        )
        .unwrap();
        let store = SkillStore::open_at(&paths).unwrap();
        assert!(store.metadata(&replacement_id).unwrap().is_none());
        assert!(store.get(&replacement_id).unwrap().is_none());
        let generation = store.generation_state().unwrap();
        assert_eq!(generation.desired_generation, generation.applied_generation);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Build a same-contract sibling of the first bundled seed.
    ///
    /// Only the source differs, so the sibling keeps the seed's description and
    /// exports — the normalized contract admission refuses as `duplicate_skill`
    /// — while claiming no predecessor and therefore no shared lineage.
    fn duplicate_package_json(marker: &str) -> (String, String) {
        let mut value: serde_json::Value = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        let source = value["proposal"]["source"].as_str().unwrap().to_string();
        value["proposal"]["source"] = serde_json::Value::String(format!("{source}\n// {marker}\n"));
        let json = serde_json::to_string(&value).unwrap();
        let package: LearnedSkillPackage = serde_json::from_str(&json).unwrap();
        let skill_id = JsProposal::try_from(package.proposal)
            .unwrap()
            .validate_and_canonicalize()
            .unwrap()
            .id;
        (json, skill_id)
    }

    /// Build a replacement package for the first bundled seed.
    ///
    /// Only the source gains a distinguishing comment, so the candidate keeps
    /// every inherited test, tag, export and capability of its predecessor and
    /// therefore clears admission's inherited-regression and held-out gates.
    fn replacement_package_json(marker: &str, predecessor_id: &str) -> (String, String) {
        let mut value: serde_json::Value = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        let source = value["proposal"]["source"].as_str().unwrap().to_string();
        value["proposal"]["source"] = serde_json::Value::String(format!("{source}\n// {marker}\n"));
        value["proposal"]["predecessor_id"] = serde_json::Value::String(predecessor_id.to_string());
        let json = serde_json::to_string(&value).unwrap();
        let package: LearnedSkillPackage = serde_json::from_str(&json).unwrap();
        let skill_id = JsProposal::try_from(package.proposal)
            .unwrap()
            .validate_and_canonicalize()
            .unwrap()
            .id;
        (json, skill_id)
    }

    fn import_active_root(paths: &AppPaths) -> String {
        let package: LearnedSkillPackage = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        let root_id = JsProposal::try_from(package.proposal.clone())
            .unwrap()
            .validate_and_canonicalize()
            .unwrap()
            .id;
        import_package(package, paths, None, "promotion-root").unwrap();
        review_proposal(&root_id, true, paths, None).unwrap();
        activate_skill(&root_id, paths, None).unwrap();
        root_id
    }

    fn import_replacement(paths: &AppPaths, marker: &str, predecessor_id: &str) -> String {
        let (json, skill_id) = replacement_package_json(marker, predecessor_id);
        let package: LearnedSkillPackage = serde_json::from_str(&json).unwrap();
        import_package(package, paths, None, marker).unwrap();
        skill_id
    }

    fn approved_replacement(paths: &AppPaths, marker: &str, predecessor_id: &str) -> String {
        let skill_id = import_replacement(paths, marker, predecessor_id);
        review_proposal(&skill_id, true, paths, None).unwrap();
        skill_id
    }

    fn revision_state(paths: &AppPaths, skill_id: &str) -> super::super::lifecycle::RevisionState {
        let mut store = SkillStore::open_at(paths).unwrap();
        LifecycleService::new(&mut store)
            .revision(skill_id)
            .unwrap()
    }

    #[test]
    fn operator_promotion_supersedes_an_active_predecessor() {
        let (root, paths, _) = fixture();
        let predecessor_id = import_active_root(&paths);
        let candidate_id = approved_replacement(&paths, "replacement-over-active", &predecessor_id);
        assert_eq!(
            revision_state(&paths, &candidate_id).status,
            LifecycleStatus::Canary
        );

        run(
            None,
            false,
            None,
            Some(LibraryOperation::Promote(&candidate_id)),
            &paths,
            None,
        )
        .unwrap();

        let candidate = revision_state(&paths, &candidate_id);
        let predecessor = revision_state(&paths, &predecessor_id);
        assert_eq!(candidate.status, LifecycleStatus::Active);
        assert_eq!(predecessor.status, LifecycleStatus::Superseded);
        // Lineage is preserved in both directions, unlike the purge workaround.
        assert_eq!(candidate.supersedes_id.as_deref(), Some(&*predecessor_id));
        assert_eq!(
            predecessor.superseded_by_id.as_deref(),
            Some(&*candidate_id)
        );
        assert_eq!(candidate.lineage_root_id, predecessor.lineage_root_id);

        let store = SkillStore::open_at(&paths).unwrap();
        assert!(store.is_retrievable(&candidate_id).unwrap());
        assert!(!store.is_retrievable(&predecessor_id).unwrap());
        let generation = store.generation_state().unwrap();
        assert_eq!(generation.desired_generation, generation.applied_generation);
        // The recorded transition names the operator action, not an evidence
        // threshold, and is bound to a second local-owner approval.
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM skill_transitions
                      WHERE skill_id = ? AND to_status = 'active' AND reason = ?",
                    rusqlite::params![candidate_id, OPERATOR_PROMOTION_REASON],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM skill_lifecycle_approvals
                      WHERE skill_id = ? AND approval_kind = 'phase5_operator_promotion'",
                    [&candidate_id],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM skill_evidence
                      WHERE skill_id = ? AND evidence_kind = 'operator_promotion'",
                    [&candidate_id],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn operator_promotion_replaces_a_quarantined_predecessor() {
        let (root, paths, _) = fixture();
        let predecessor_id = import_active_root(&paths);
        let candidate_id = approved_replacement(&paths, "emergency-replacement", &predecessor_id);

        // The emergency sequence: the defective active skill is quarantined
        // first, which used to make its replacement unpromotable forever.
        run(
            None,
            false,
            Some(FeedbackOperation {
                skill_id: &predecessor_id,
                invocation_id: None,
                kind: "severe",
                reason_code: "integrity",
                idempotency_key: "operator-promotion-quarantine",
            }),
            None,
            &paths,
            None,
        )
        .unwrap();
        assert_eq!(
            revision_state(&paths, &predecessor_id).status,
            LifecycleStatus::Quarantined
        );

        run(
            None,
            false,
            None,
            Some(LibraryOperation::Promote(&candidate_id)),
            &paths,
            None,
        )
        .unwrap();

        let candidate = revision_state(&paths, &candidate_id);
        let predecessor = revision_state(&paths, &predecessor_id);
        assert_eq!(candidate.status, LifecycleStatus::Active);
        assert_eq!(predecessor.status, LifecycleStatus::Superseded);
        assert_eq!(
            predecessor.superseded_by_id.as_deref(),
            Some(&*candidate_id)
        );
        let store = SkillStore::open_at(&paths).unwrap();
        assert!(store.is_retrievable(&candidate_id).unwrap());
        let generation = store.generation_state().unwrap();
        assert_eq!(generation.desired_generation, generation.applied_generation);
        drop(store);

        // A repeat promotion is a no-op rather than a second transition.
        run(
            None,
            false,
            None,
            Some(LibraryOperation::Promote(&candidate_id)),
            &paths,
            None,
        )
        .unwrap();
        assert_eq!(
            revision_state(&paths, &candidate_id).status,
            LifecycleStatus::Active
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn operator_promotion_refuses_roots_non_canaries_and_ineligible_predecessors() {
        let (root, paths, _) = fixture();
        let predecessor_id = import_active_root(&paths);

        let rootless = promote_replacement_skill(&predecessor_id, &paths, None)
            .unwrap_err()
            .to_string();
        assert!(rootless.contains("lineage root"), "{rootless}");
        assert!(rootless.contains("--activate-learned-skill"), "{rootless}");

        let unapproved_id = import_replacement(&paths, "unapproved-replacement", &predecessor_id);
        let not_canary = promote_replacement_skill(&unapproved_id, &paths, None)
            .unwrap_err()
            .to_string();
        assert!(not_canary.contains("approved canary"), "{not_canary}");
        assert!(not_canary.contains("verified"), "{not_canary}");
        assert_ne!(rootless, not_canary);

        let candidate_id = approved_replacement(&paths, "ineligible-predecessor", &predecessor_id);
        let mut store = SkillStore::open_at(&paths).unwrap();
        store
            .conn_mut()
            .execute(
                "UPDATE skill_revisions SET status = 'retired' WHERE id = ?",
                [&predecessor_id],
            )
            .unwrap();
        drop(store);
        let ineligible = promote_replacement_skill(&candidate_id, &paths, None)
            .unwrap_err()
            .to_string();
        assert!(
            ineligible.contains("active or quarantined predecessor"),
            "{ineligible}"
        );
        assert!(ineligible.contains("retired"), "{ineligible}");
        assert_ne!(ineligible, not_canary);
        assert_eq!(
            revision_state(&paths, &candidate_id).status,
            LifecycleStatus::Canary
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn proposal_queue_lists_pending_operator_decisions_and_omits_terminal_ones() {
        let (root, paths, _) = fixture();
        let package: LearnedSkillPackage = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        let skill_id = JsProposal::try_from(package.proposal.clone())
            .unwrap()
            .validate_and_canonicalize()
            .unwrap()
            .id;
        import_package(package, &paths, None, "queue-listing").unwrap();

        let store = SkillStore::open_at(&paths).unwrap();
        let rows = load_proposal_queue(&store).unwrap();
        let listed = rows
            .iter()
            .find(|row| row.skill_id == skill_id)
            .expect("a proposal awaiting approval must be listed");
        assert_eq!(listed.status, "awaiting_approval");
        assert_eq!(listed.reason_code, None);
        assert!(listed.report_id.is_some());
        let line = listed.to_line();
        assert!(line.contains(&listed.proposal_id), "{line}");
        assert!(line.contains(&skill_id), "{line}");
        assert!(line.contains("awaiting_approval"), "{line}");
        // An absent reason code renders as the documented placeholder.
        assert!(line.contains("\t-\t"), "{line}");
        assert!(line.ends_with(&format!("\t{}\t{}", listed.created_at, listed.updated_at)));
        drop(store);

        let unavailable = unavailable_embedding_config();
        assert!(
            review_proposal(&skill_id, true, &paths, Some(&unavailable)).is_err(),
            "approval must still honor the configured embedding backend"
        );
        assert_eq!(
            SkillStore::open_at(&paths)
                .unwrap()
                .get_proposal(&skill_id)
                .unwrap()
                .unwrap()
                .status,
            ProposalStatus::AwaitingApproval
        );
        review_proposal(&skill_id, false, &paths, Some(&unavailable))
            .expect("human rejection must not require embedding credentials");
        let store = SkillStore::open_at(&paths).unwrap();
        assert!(
            load_proposal_queue(&store)
                .unwrap()
                .iter()
                .all(|row| row.skill_id != skill_id),
            "a rejected proposal is terminal and must not be listed"
        );
        // The terminal outcome is still reachable by identifier, which is the
        // only way an operator can see why admission ended where it did.
        let rejected = load_proposal(&store, &skill_id)
            .unwrap()
            .expect("a rejected proposal must remain queryable by id");
        assert_eq!(rejected.status, "rejected");
        assert_eq!(rejected.skill_id, skill_id);
        assert!(load_proposal(&store, &"e".repeat(64)).unwrap().is_none());
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn every_bundled_seed_passes_its_contained_held_out_baseline() {
        let (root, paths, _) = fixture();
        for (name, source) in SEED_PACKAGES {
            let package: LearnedSkillPackage = serde_json::from_str(source).unwrap();
            let skill_id = JsProposal::try_from(package.proposal.clone())
                .unwrap()
                .validate_and_canonicalize()
                .unwrap()
                .id;
            import_package(package, &paths, None, name).unwrap();
            let proposal = SkillStore::open_at(&paths)
                .unwrap()
                .get_proposal(&skill_id)
                .unwrap()
                .unwrap();
            assert_eq!(proposal.status, ProposalStatus::AwaitingApproval, "{name}");
            assert!(proposal.reason_code.is_none(), "{name}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[allow(unsafe_code)]
    fn package_reader_requires_regular_files_and_bounds_content() {
        let (root, _paths, _) = fixture();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("package.json");
        let mut payload = SEED_PACKAGES[0].1.to_string();
        payload.extend(std::iter::repeat_n(
            ' ',
            MAX_PACKAGE_BYTES as usize - payload.len(),
        ));
        std::fs::write(&path, &payload).unwrap();
        let package = read_package(&path).expect("the exact byte limit is accepted");
        validate_package(&package, "test").unwrap();
        payload.push(' ');
        std::fs::write(&path, &payload).unwrap();
        let error = read_package(&path).err().expect("oversized package");
        assert!(error.to_string().contains("exceeds 256 KiB"), "{error}");

        std::fs::write(&path, SEED_PACKAGES[0].1).unwrap();
        assert!(read_package(&root).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let link = root.join("link.json");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(
                read_package(&link).is_err(),
                "the read itself must reject a file swapped for a symlink"
            );

            for replacement in ["file", "symlink", "fifo"] {
                let checked = root.join(format!("swap-{replacement}.json"));
                std::fs::write(&checked, SEED_PACKAGES[0].1).unwrap();
                let target = path.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                let reader = std::thread::spawn(move || {
                    let changed = checked.clone();
                    BEFORE_PACKAGE_OPEN.with(|slot| {
                        *slot.borrow_mut() = Some(Box::new(move || {
                            std::fs::remove_file(&changed).unwrap();
                            match replacement {
                                "file" => std::fs::write(&changed, SEED_PACKAGES[0].1).unwrap(),
                                "symlink" => std::os::unix::fs::symlink(target, changed).unwrap(),
                                "fifo" => {
                                    let name =
                                        std::ffi::CString::new(changed.as_os_str().as_bytes())
                                            .unwrap();
                                    // SAFETY: the path is NUL-terminated and lives through this call.
                                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                                }
                                _ => unreachable!(),
                            }
                        }));
                    });
                    let _ = tx.send(read_package(&checked).map(|_| ()));
                });
                let result = rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("package reading must not wait for a FIFO writer");
                reader.join().unwrap();
                assert!(result.is_err(), "accepted replacement: {replacement}");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_import_validates_every_package_before_mutating_the_store() {
        let (root, paths, _) = fixture();
        let import_dir = root.join("imports");
        std::fs::create_dir_all(&import_dir).unwrap();
        std::fs::write(import_dir.join("01-valid.json"), SEED_PACKAGES[0].1).unwrap();
        std::fs::write(import_dir.join("02-invalid.json"), b"{not-json").unwrap();
        let package: LearnedSkillPackage = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        let skill_id = JsProposal::try_from(package.proposal)
            .unwrap()
            .validate_and_canonicalize()
            .unwrap()
            .id;

        assert!(import_path(&import_dir, &paths, None).is_err());
        assert!(
            SkillStore::open_at(&paths)
                .unwrap()
                .get_proposal(&skill_id)
                .unwrap()
                .is_none()
        );
        // Directory capacity is checked before parsing any package or
        // importing the valid first entry.
        for index in 3..=MAX_DIRECTORY_PACKAGES + 1 {
            std::fs::write(import_dir.join(format!("{index:02}.json")), b"{not-json").unwrap();
        }
        let error = import_path(&import_dir, &paths, None).unwrap_err();
        assert!(error.to_string().contains("1 to 32 regular JSON files"));
        assert!(
            SkillStore::open_at(&paths)
                .unwrap()
                .get_proposal(&skill_id)
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_blocked_import_reports_a_queued_proposal_without_draining_the_other_one() {
        let (root, paths, _) = fixture();
        let neighbour = SkillArtifact::new(
            "function run() { return 3; }".into(),
            "Agent-originated queue neighbour".into(),
            vec![],
            vec![SkillExport {
                name: "run".into(),
                signature: "() => number".into(),
            }],
            vec!["run() === 3".into()],
            CapabilityManifest::pure(),
        )
        .unwrap();
        let mut store = SkillStore::open_at(&paths).unwrap();
        // Enqueued far in the past, so it owns the head of the due queue.
        store.enqueue_proposal(&neighbour, None, 1).unwrap();
        drop(store);

        let package: LearnedSkillPackage = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        let report = import_package_within(
            package,
            &paths,
            None,
            "blocked-import",
            Duration::from_secs(30),
        )
        .unwrap();

        // A proposal that is merely queued is not an import failure.
        assert_eq!(report.status, ProposalStatus::Pending);
        assert_eq!(report.blocked_by.as_deref(), Some(neighbour.id.as_str()));
        let store = SkillStore::open_at(&paths).unwrap();
        let untouched = store.get_proposal(&neighbour.id).unwrap().unwrap();
        assert_eq!(untouched.status, ProposalStatus::Pending);
        assert_eq!(
            untouched.attempt_count, 0,
            "an operator import must not spend an unrelated proposal's retry budget"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_seeds_reports_every_seed_and_continues_past_a_failing_one() {
        let (root, paths, _) = fixture();
        // A purged identity can never be re-proposed, so this seed fails for
        // good while the rest of the library is still installable.
        let package: LearnedSkillPackage = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        let purged = JsProposal::try_from(package.proposal)
            .unwrap()
            .validate_and_canonicalize()
            .unwrap();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&purged).unwrap();
        RetentionService::new(&mut store)
            .privacy_purge(&purged.id, "test_request", 10)
            .unwrap();
        drop(store);

        install_seeds(&paths, None).unwrap();

        let store = SkillStore::open_at(&paths).unwrap();
        assert!(
            store.get_proposal(&purged.id).unwrap().is_none(),
            "a purged seed stays purged"
        );
        let admitted: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM skill_proposals WHERE status = 'awaiting_approval'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            admitted,
            (SEED_PACKAGES.len() - 1) as i64,
            "one failing seed must not abort the remaining seeds"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn operator_reports_render_one_shape_in_text_and_json() {
        let report = OperatorReport::new("approve")
            .with("id", "abc")
            .with("status", "canary")
            .with("generation", 4u64)
            .with("idempotent", true)
            .with("reason_code", None::<String>);
        assert_eq!(
            report.render(false),
            "learned-skill approve: id=abc status=canary generation=4 idempotent=true reason_code=-"
        );
        let json: serde_json::Value = serde_json::from_str(&report.render(true)).unwrap();
        assert_eq!(json["command"], "approve");
        assert_eq!(json["id"], "abc");
        assert_eq!(json["generation"], 4);
        assert_eq!(json["idempotent"], true);
        assert!(json["reason_code"].is_null());
    }

    #[test]
    fn an_idempotent_approval_reports_the_live_status_and_published_generation() {
        let replay = super::super::store::CanaryApprovalResult {
            skill_id: "a".repeat(64),
            generation: 2,
            idempotent: true,
        };
        // The revision moved on after the first approval: the report must say
        // so instead of restating "approved as canary" at generation 2.
        let line = approve_report(&replay, 9, "active").render(false);
        assert!(line.contains("idempotent=true"), "{line}");
        assert!(line.contains("status=active"), "{line}");
        assert!(line.contains("generation=9"), "{line}");
        assert!(line.contains("approval_generation=2"), "{line}");

        let first = super::super::store::CanaryApprovalResult {
            skill_id: "a".repeat(64),
            generation: 9,
            idempotent: false,
        };
        assert!(
            approve_report(&first, 9, "canary")
                .render(false)
                .contains("idempotent=false")
        );
    }

    #[test]
    fn purge_refuses_a_non_terminal_revision_without_the_force_flag() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        drop(store);

        let refused = run(
            Some(PurgeOperation {
                skill_id: &artifact.id,
                force: false,
            }),
            false,
            None,
            None,
            &paths,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("terminal lifecycle status"), "{refused}");
        assert!(refused.contains("--purge-learned-skill-force"), "{refused}");
        assert!(
            SkillStore::open_at(&paths)
                .unwrap()
                .get(&artifact.id)
                .unwrap()
                .is_some(),
            "a refused purge must not delete anything"
        );

        run(
            Some(PurgeOperation {
                skill_id: &artifact.id,
                force: true,
            }),
            false,
            None,
            None,
            &paths,
            None,
        )
        .unwrap();
        assert!(
            SkillStore::open_at(&paths)
                .unwrap()
                .get(&artifact.id)
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn purge_names_the_dependants_a_terminal_target_would_reroot() {
        let (root, paths, artifact) = fixture();
        let replacement = SkillArtifact::new(
            "function run() { return 1; } // replacement".into(),
            "Operator surface replacement".into(),
            vec![],
            vec![SkillExport {
                name: "run".into(),
                signature: "() => number".into(),
            }],
            vec!["run() === 1".into()],
            CapabilityManifest::pure(),
        )
        .unwrap();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        store.insert_verified(&replacement).unwrap();
        store
            .conn_mut()
            .execute(
                "UPDATE skill_revisions SET status = 'superseded' WHERE id = ?",
                [&artifact.id],
            )
            .unwrap();
        store
            .conn_mut()
            .execute(
                "UPDATE skill_revisions
                 SET status = 'canary', supersedes_id = ? WHERE id = ?",
                rusqlite::params![artifact.id, replacement.id],
            )
            .unwrap();
        drop(store);

        // The target itself is terminal, so only the dependant forces the
        // operator's hand.
        let refused = run(
            Some(PurgeOperation {
                skill_id: &artifact.id,
                force: false,
            }),
            false,
            None,
            None,
            &paths,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("re-root"), "{refused}");
        assert!(refused.contains(&replacement.id), "{refused}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn severe_feedback_reports_whether_containment_actually_happened() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        store
            .conn_mut()
            .execute(
                "UPDATE skill_revisions SET status = 'verified' WHERE id = ?",
                [&artifact.id],
            )
            .unwrap();
        drop(store);
        let operation = FeedbackOperation {
            skill_id: &artifact.id,
            invocation_id: None,
            kind: "severe",
            reason_code: "integrity",
            idempotency_key: "severe-ineligible",
        };

        let attribution = FeedbackAttribution::new("feedback-1", "integrity");
        let store = SkillStore::open_at(&paths).unwrap();
        let (outcome, detail) =
            contain_severe_feedback(store, &operation, &attribution, &paths, None, 100).unwrap();
        assert_eq!(outcome, "skipped");
        assert_eq!(detail.as_deref(), Some("ineligible_status:verified"));
        assert_eq!(
            SkillStore::open_at(&paths)
                .unwrap()
                .metadata(&artifact.id)
                .unwrap()
                .unwrap()
                .status,
            "verified",
            "an ineligible target must not be quarantined"
        );

        let mut store = SkillStore::open_at(&paths).unwrap();
        store
            .conn_mut()
            .execute(
                "UPDATE skill_revisions SET status = 'active' WHERE id = ?",
                [&artifact.id],
            )
            .unwrap();
        drop(store);
        let store = SkillStore::open_at(&paths).unwrap();
        let (outcome, detail) =
            contain_severe_feedback(store, &operation, &attribution, &paths, None, 101).unwrap();
        assert_eq!(outcome, "applied");
        assert_eq!(detail, None);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A proposal parked by the attempt-exhaustion sweep is recoverable without
    /// re-importing the byte-identical package, which is the only route an
    /// operator has for a proposal the model authored.
    #[test]
    fn reevaluation_requeues_a_proposal_parked_by_the_attempt_sweep() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        let queued = store
            .enqueue_proposal(&artifact, None, current_timestamp().unwrap())
            .unwrap();
        store
            .conn_mut()
            .execute(
                "UPDATE skill_proposals SET attempt_count = ?1 WHERE proposal_id = ?2",
                rusqlite::params![
                    crate::extras::js::skills::store::MAX_EVALUATION_ATTEMPTS,
                    &queued.proposal_id
                ],
            )
            .unwrap();
        let parked = store
            .sweep_exhausted_proposals(current_timestamp().unwrap())
            .unwrap();
        assert_eq!(parked, vec![queued.proposal_id.clone()]);
        let before = store.get_proposal(&queued.proposal_id).unwrap().unwrap();
        assert_eq!(before.status, ProposalStatus::Deferred);
        drop(store);

        let unavailable = unavailable_embedding_config();
        run(
            None,
            false,
            None,
            Some(LibraryOperation::Reevaluate(&queued.proposal_id)),
            &paths,
            Some(&unavailable),
        )
        .expect("a parked proposal must be requeueable");

        let store = SkillStore::open_at(&paths).unwrap();
        let after = store.get_proposal(&queued.proposal_id).unwrap().unwrap();
        assert_eq!(
            after.status,
            ProposalStatus::Pending,
            "re-evaluation must return the proposal to the queue"
        );
        assert_eq!(after.reason_code, None);
        assert_eq!(after.attempt_count, 0);
        assert!(
            run(
                None,
                false,
                None,
                Some(LibraryOperation::Reevaluate(&queued.proposal_id)),
                &paths,
                Some(&unavailable)
            )
            .is_err(),
            "an already pending row is not parked"
        );
        assert_eq!(
            store.get_proposal(&queued.proposal_id).unwrap().unwrap(),
            after
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reevaluation_refreshes_stale_reports_before_approval() {
        for change in ["verifier", "corpus"] {
            let (root, paths, _) = fixture();
            let package =
                || serde_json::from_str::<LearnedSkillPackage>(SEED_PACKAGES[0].1).unwrap();
            let imported = import_package(package(), &paths, None, "refresh").unwrap();
            let skill_id = imported.skill_id;
            let mut store = SkillStore::open_at(&paths).unwrap();
            let initial = store.get_proposal(&skill_id).unwrap().unwrap();
            if change == "verifier" {
                let original_id = initial.report_id.as_deref().unwrap();
                let mut old = store.get_evaluation_report(original_id).unwrap().unwrap();
                old.verifier_version -= 1;
                old.report_id = old.recompute_id().unwrap();
                let tx = store.conn_mut().transaction().unwrap();
                tx
                    .execute(
                        "UPDATE evaluation_reports SET verifier_version = ?, report_id = ? WHERE report_id = ?",
                        rusqlite::params![old.verifier_version, old.report_id, original_id],
                    )
                    .unwrap();
                tx.execute(
                    "UPDATE skill_proposals SET report_id = ? WHERE proposal_id = ?",
                    rusqlite::params![old.report_id, skill_id],
                )
                .unwrap();
                tx.commit().unwrap();
            } else {
                let mut suite = package().held_out_suites.remove(0);
                suite.cases[0].expression = format!("({})", suite.cases[0].expression);
                suite
                    .import(
                        &mut store,
                        &AdminIdentity::authenticated("local-owner").unwrap(),
                        current_timestamp().unwrap(),
                    )
                    .unwrap();
            }
            let before = store.get_proposal(&skill_id).unwrap().unwrap();
            let old_report_id = before.report_id.as_deref().unwrap();
            assert!(review_proposal(&skill_id, true, &paths, None).is_err());
            let old_report = store.get_evaluation_report(old_report_id).unwrap().unwrap();
            let admin = AdminIdentity::authenticated("local-owner").unwrap();
            for (authority, version) in [
                (None, before.row_version),
                (Some(&admin), before.row_version - 1),
            ] {
                assert!(
                    store
                        .request_blocked_reevaluation(
                            authority,
                            &skill_id,
                            version,
                            current_timestamp().unwrap()
                        )
                        .is_err()
                );
                assert_eq!(store.get_proposal(&skill_id).unwrap().unwrap(), before);
            }
            drop(store);

            reevaluate_skill(&skill_id, &paths).expect("an unapproved stale report is refreshable");
            let store = SkillStore::open_at(&paths).unwrap();
            let pending = store.get_proposal(&skill_id).unwrap().unwrap();
            assert_eq!(pending.status, ProposalStatus::Pending);
            assert_eq!(pending.report_id, None);
            assert_eq!(pending.attempt_count, before.attempt_count);
            assert_eq!(
                store.revision_status(&skill_id).unwrap().as_deref(),
                Some("pending")
            );
            assert_eq!(store.desired_generation().unwrap(), 0);
            assert_eq!(
                store.get_evaluation_report(old_report_id).unwrap().unwrap(),
                old_report
            );
            drop(store);

            import_package(package(), &paths, None, "refresh").unwrap();
            let store = SkillStore::open_at(&paths).unwrap();
            let refreshed = store.get_proposal(&skill_id).unwrap().unwrap();
            assert_eq!(refreshed.status, ProposalStatus::AwaitingApproval);
            assert_eq!(refreshed.attempt_count, before.attempt_count + 1);
            assert_ne!(refreshed.report_id, before.report_id);
            drop(store);
            if change == "verifier" {
                // Several explicit verifier/corpus refreshes may outlive one
                // claim budget; their durable report numbers must never rewind.
                for expected_attempt in 3..=super::super::store::MAX_EVALUATION_ATTEMPTS + 2 {
                    reevaluate_skill(&skill_id, &paths).unwrap();
                    import_package(package(), &paths, None, "refresh").unwrap();
                    let store = SkillStore::open_at(&paths).unwrap();
                    let proposal = store.get_proposal(&skill_id).unwrap().unwrap();
                    assert_eq!(proposal.status, ProposalStatus::AwaitingApproval);
                    let report = store
                        .get_evaluation_report(proposal.report_id.as_deref().unwrap())
                        .unwrap()
                        .unwrap();
                    assert_eq!(report.attempt, expected_attempt);
                    let count: u32 = store
                        .conn()
                        .query_row(
                            "SELECT COUNT(*) FROM evaluation_reports WHERE proposal_id = ?",
                            [&skill_id],
                            |row| row.get(0),
                        )
                        .unwrap();
                    assert_eq!(count, expected_attempt);
                }
            }
            review_proposal(&skill_id, true, &paths, None).unwrap();
            assert!(reevaluate_skill(&skill_id, &paths).is_err());
            assert_eq!(live_revision_status(&paths, &skill_id).unwrap(), "canary");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn retirement_disables_an_active_skill_and_keeps_its_lineage() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        drop(store);

        run(
            None,
            false,
            None,
            Some(LibraryOperation::Retire(&artifact.id)),
            &paths,
            None,
        )
        .unwrap();

        let store = SkillStore::open_at(&paths).unwrap();
        assert_eq!(
            store.revision_status(&artifact.id).unwrap().as_deref(),
            Some("retired")
        );
        assert!(!store.is_retrievable(&artifact.id).unwrap());
        // Unlike purge, the revision and its bytes survive.
        assert!(store.get(&artifact.id).unwrap().is_some());
        drop(store);

        // A repeated retirement is an acknowledged no-op, not an error.
        retire_skill(&artifact.id, &paths, None).unwrap();
        assert_eq!(
            SkillStore::open_at(&paths)
                .unwrap()
                .revision_status(&artifact.id)
                .unwrap()
                .as_deref(),
            Some("retired")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn activation_validates_the_target_before_paying_for_an_index_rebuild() {
        let (root, paths, artifact) = fixture();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        let before = store.generation_state().unwrap();
        drop(store);

        let error = activate_skill(&"a".repeat(64), &paths, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("proposal not found"), "{error}");

        let after = SkillStore::open_at(&paths)
            .unwrap()
            .generation_state()
            .unwrap();
        assert_eq!(
            (after.desired_generation, after.applied_generation),
            (before.desired_generation, before.applied_generation),
            "a mistyped identifier must be rejected before the embedding rebuild"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_import_wait_polls_its_own_proposal_and_never_claims_another() {
        let (root, paths, artifact) = fixture();
        let other = SkillArtifact::new(
            "function run() { return 2; }".into(),
            "Operator surface queue neighbour".into(),
            vec![],
            vec![SkillExport {
                name: "run".into(),
                signature: "() => number".into(),
            }],
            vec!["run() === 2".into()],
            CapabilityManifest::pure(),
        )
        .unwrap();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.enqueue_proposal(&other, None, 10).unwrap();
        store.enqueue_proposal(&artifact, None, 20).unwrap();

        // `evaluate_next` claims the oldest due proposal, so the neighbour is
        // the head and an import of the later proposal must not drive it.
        assert_eq!(
            due_proposal_head(&store, 30).unwrap().as_deref(),
            Some(other.id.as_str())
        );
        store
            .conn_mut()
            .execute(
                "UPDATE skill_proposals SET next_attempt_at = 900 WHERE skill_id = ?",
                [&other.id],
            )
            .unwrap();
        assert_eq!(
            due_proposal_head(&store, 30).unwrap().as_deref(),
            Some(artifact.id.as_str())
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_import_wait_sleeps_to_the_backoff_without_overrunning_its_budget() {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        // No scheduled retry: poll at the short cadence rather than busy-loop.
        assert_eq!(sleep_until_due(None, 100, deadline), IMPORT_POLL_INTERVAL);
        // A one-second backoff is waited out rather than spun through.
        assert_eq!(
            sleep_until_due(Some(101), 100, deadline),
            Duration::from_secs(1)
        );
        // A long backoff is capped so the budget stays enforceable.
        assert_eq!(
            sleep_until_due(Some(100_000), 100, deadline),
            IMPORT_MAX_SLEEP
        );
        // An expired budget never sleeps.
        let expired = std::time::Instant::now() - Duration::from_secs(1);
        assert_eq!(sleep_until_due(Some(100_000), 100, expired), Duration::ZERO);
    }

    #[test]
    fn a_replacement_import_names_the_predecessors_observed_state() {
        let (root, paths, artifact) = fixture();
        let (json, _) = replacement_package_json("absent-predecessor", &"b".repeat(64));
        let package: LearnedSkillPackage = serde_json::from_str(&json).unwrap();
        // The pre-flight refuses before any held-out baseline is written.
        let absent = import_package(package, &paths, None, "absent-predecessor")
            .unwrap_err()
            .to_string();
        assert!(absent.contains("cannot be enqueued"), "{absent}");
        assert_eq!(
            SkillStore::open_at(&paths)
                .unwrap()
                .connection()
                .query_row("SELECT COUNT(*) FROM held_out_suites", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0,
            "a refused enqueue must not leave orphan trusted baselines behind"
        );

        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(&artifact).unwrap();
        store
            .conn_mut()
            .execute(
                "UPDATE skill_revisions SET status = 'retired' WHERE id = ?",
                [&artifact.id],
            )
            .unwrap();
        drop(store);
        let (json, _) = replacement_package_json("retired-predecessor", &artifact.id);
        let package: LearnedSkillPackage = serde_json::from_str(&json).unwrap();
        let candidate = JsProposal::try_from(package.proposal)
            .unwrap()
            .validate_and_canonicalize()
            .unwrap();
        let store = SkillStore::open_at(&paths).unwrap();
        let ineligible = preflight_enqueue(&store, &candidate, Some(&artifact.id))
            .unwrap_err()
            .to_string();
        assert!(ineligible.contains("is retired"), "{ineligible}");
        let missing = preflight_enqueue(&store, &candidate, Some(&"c".repeat(64)))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("absent from the store"), "{missing}");
        assert_ne!(ineligible, missing);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn local_owner_route_imports_approves_and_activates_a_verified_seed() {
        let (root, paths, _) = fixture();
        let package: LearnedSkillPackage = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        let skill_id = JsProposal::try_from(package.proposal.clone())
            .unwrap()
            .validate_and_canonicalize()
            .unwrap()
            .id;

        let admin = AdminIdentity::authenticated("local-owner").unwrap();
        let mut store = SkillStore::open_at(&paths).unwrap();
        let baseline = package.held_out_suites[0].clone();
        let baseline_id = baseline.clone().import(&mut store, &admin, 1).unwrap();
        let mut extra_id = String::new();
        for index in 0..32 {
            let mut extra = baseline.clone();
            extra.cases.truncate(1);
            extra.cases[0]
                .expression
                .push_str(&format!(" /* extra {index} */"));
            extra_id = extra.import(&mut store, &admin, 1).unwrap();
        }
        drop(store);
        import_package_within(package, &paths, None, "test-seed", Duration::ZERO).unwrap();
        let mut evaluator = AdmissionEvaluator::new(
            SkillStore::open_at(&paths).unwrap(),
            Arc::new(Embedder::new().unwrap()),
            "corpus-recovery-test",
        )
        .unwrap();
        let mut now = current_timestamp().unwrap();
        for attempt in 1..=MAX_EVALUATION_ATTEMPTS {
            assert!(matches!(
                evaluator.evaluate_next(now),
                Err(super::super::admission::AdmissionError::Retryable(_))
            ));
            let proposal = evaluator.store().get_proposal(&skill_id).unwrap().unwrap();
            if attempt < MAX_EVALUATION_ATTEMPTS {
                now = proposal.next_attempt_at.unwrap();
            }
        }
        assert_eq!(
            evaluator
                .store()
                .get_proposal(&skill_id)
                .unwrap()
                .unwrap()
                .status,
            ProposalStatus::Deferred
        );
        let listing: serde_json::Value = serde_json::from_str(
            &suite_listing_report(evaluator.store())
                .unwrap()
                .render(true),
        )
        .unwrap();
        let entries = listing["suites"].as_array().unwrap();
        assert_eq!(entries.len(), 33);
        for entry in entries {
            assert_eq!(
                entry.as_object().unwrap().len(),
                2,
                "listing exposes only ID and enabled state"
            );
            assert_eq!(entry["id"].as_str().unwrap().len(), 64);
            assert_eq!(entry["enabled"], true);
        }
        drop(evaluator);
        run(
            None,
            false,
            None,
            Some(LibraryOperation::ListSuites),
            &paths,
            None,
        )
        .unwrap();
        for _ in 0..2 {
            run(
                None,
                false,
                None,
                Some(LibraryOperation::DisableSuite(&extra_id)),
                &paths,
                None,
            )
            .unwrap();
        }
        assert!(
            run(
                None,
                false,
                None,
                Some(LibraryOperation::DisableSuite(&"0".repeat(64))),
                &paths,
                None
            )
            .is_err()
        );
        run(
            None,
            false,
            None,
            Some(LibraryOperation::Reevaluate(&skill_id)),
            &paths,
            None,
        )
        .unwrap();
        let store = SkillStore::open_at(&paths).unwrap();
        assert_eq!(
            store.get_proposal(&skill_id).unwrap().unwrap().status,
            ProposalStatus::Pending
        );
        assert!(
            store
                .held_out_suite_states()
                .unwrap()
                .contains(&(extra_id.clone(), false))
        );
        drop(store);
        let package = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        import_package(package, &paths, None, "test-seed").unwrap();
        assert_eq!(
            SkillStore::open_at(&paths)
                .unwrap()
                .get_proposal(&skill_id)
                .unwrap()
                .unwrap()
                .status,
            ProposalStatus::AwaitingApproval
        );
        // An authenticated package reimport must repair a damaged baseline
        // before the existing proposal can pass the human approval gate.
        let store = SkillStore::open_at(&paths).unwrap();
        store
            .connection()
            .execute(
                "UPDATE held_out_suites SET cases_json = '[]' WHERE suite_id = ?1",
                [&baseline_id],
            )
            .unwrap();
        drop(store);
        assert!(review_proposal(&skill_id, true, &paths, None).is_err());
        let package = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        import_package(package, &paths, None, "test-seed-repair")
            .expect("reimport repairs the trusted baseline");
        review_proposal(&skill_id, true, &paths, None).unwrap();
        assert_eq!(
            SkillStore::open_at(&paths)
                .unwrap()
                .revision_status(&skill_id)
                .unwrap()
                .as_deref(),
            Some("canary")
        );
        assert_rejection_preserves_approved_state(&paths, &skill_id);
        review_proposal(&skill_id, true, &paths, None)
            .expect("explicit approval replay remains idempotent");
        activate_skill(&skill_id, &paths, None).unwrap();
        let store = SkillStore::open_at(&paths).unwrap();
        assert_eq!(
            store.revision_status(&skill_id).unwrap().as_deref(),
            Some("active")
        );
        assert!(store.is_retrievable(&skill_id).unwrap());
        drop(store);
        assert_rejection_preserves_approved_state(&paths, &skill_id);
        activate_skill(&skill_id, &paths, None).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
