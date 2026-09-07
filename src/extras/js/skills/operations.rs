//! Explicit local-owner surfaces for the learned-skill lifecycle.
//!
//! These commands run before provider initialization. Purge uses the same
//! coordinator transaction/publication gate as lifecycle removals; compaction
//! never deletes raw events until their daily aggregates and watermark commit.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;

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
    QuarantineEvidence, QuarantineExecutor, QuarantinePolicy, QuarantineReason,
};
use super::retention::{CoordinatedRetention, DEFAULT_RAW_RETENTION_SECONDS, RetentionService};
use super::store::{AdminIdentity, ProposalStatus, SkillStore, current_timestamp};
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
    declared_effect_methods: u64,
    estimated_round_trips_saved: u64,
}

impl SkillUsageStats {
    fn success_percent(&self) -> f64 {
        let terminals = self.direct_successes.saturating_add(self.direct_failures);
        if terminals == 0 {
            0.0
        } else {
            self.direct_successes as f64 * 100.0 / terminals as f64
        }
    }

    fn pass_rate_without_percent(&self) -> f64 {
        if self.baseline_tasks == 0 {
            0.0
        } else {
            self.baseline_passes as f64 * 100.0 / self.baseline_tasks as f64
        }
    }
}

fn estimate_round_trips_saved(direct_successes: u64, declared_effect_methods: u64) -> u64 {
    // Actual effect counts are intentionally not retained with skill telemetry.
    // This lower-bound proxy credits only additional distinct declared methods.
    direct_successes.saturating_mul(declared_effect_methods.saturating_sub(1))
}

fn load_skill_stats(store: &SkillStore) -> anyhow::Result<Vec<SkillUsageStats>> {
    let mut statement = store.connection().prepare(
        "SELECT revision.id, revision.status,
                COALESCE(stats.invoked_count, 0),
                COALESCE(stats.direct_success_count, 0),
                COALESCE(stats.direct_failure_count, 0),
                (SELECT MAX(event.created_at) FROM skill_events AS event
                  WHERE event.skill_id = revision.id AND event.event_kind = 'invoked'),
                (SELECT COUNT(DISTINCT outcome.turn_id)
                   FROM skill_task_outcome_links AS link
                   JOIN skill_task_outcomes AS outcome
                     ON outcome.evidence_id = link.evidence_id
                  WHERE link.skill_id = revision.id
                    AND outcome.source_kind != 'no_verify_command'),
                (SELECT COUNT(DISTINCT CASE WHEN outcome.verify_passed = 1
                                           THEN outcome.turn_id END)
                   FROM skill_task_outcome_links AS link
                   JOIN skill_task_outcomes AS outcome
                     ON outcome.evidence_id = link.evidence_id
                  WHERE link.skill_id = revision.id
                    AND outcome.source_kind != 'no_verify_command'),
                (SELECT COUNT(DISTINCT baseline.turn_id)
                   FROM skill_task_outcomes AS baseline
                  WHERE baseline.source_kind != 'no_verify_command'
                    AND NOT EXISTS (
                        SELECT 1 FROM skill_task_outcome_links AS absent
                         WHERE absent.evidence_id = baseline.evidence_id
                    )
                    AND EXISTS (
                        SELECT 1
                          FROM skill_task_outcomes AS observed
                          JOIN skill_task_outcome_links AS observed_link
                            ON observed_link.evidence_id = observed.evidence_id
                         WHERE observed_link.skill_id = revision.id
                           AND observed.source_kind = baseline.source_kind
                           AND observed.source_id IS baseline.source_id
                    )),
                (SELECT COUNT(DISTINCT CASE WHEN baseline.verify_passed = 1
                                           THEN baseline.turn_id END)
                   FROM skill_task_outcomes AS baseline
                  WHERE baseline.source_kind != 'no_verify_command'
                    AND NOT EXISTS (
                        SELECT 1 FROM skill_task_outcome_links AS absent
                         WHERE absent.evidence_id = baseline.evidence_id
                    )
                    AND EXISTS (
                        SELECT 1
                          FROM skill_task_outcomes AS observed
                          JOIN skill_task_outcome_links AS observed_link
                            ON observed_link.evidence_id = observed.evidence_id
                         WHERE observed_link.skill_id = revision.id
                           AND observed.source_kind = baseline.source_kind
                           AND observed.source_id IS baseline.source_id
                    )),
                revision.capability_json
           FROM skill_revisions AS revision
           LEFT JOIN skill_stats AS stats ON stats.skill_id = revision.id
          WHERE revision.identity_version = 2
          ORDER BY COALESCE(stats.invoked_count, 0) DESC, revision.id",
    )?;
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
            row.get::<_, String>(10)?,
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
    println!(
        "id\tstatus\tinvocations\tsuccess\tlast_used_unix\ttasks_with\tpassed_with\tpass_rate_without\tdeclared_effect_methods\test_round_trips_saved"
    );
    for row in &rows {
        println!(
            "{}\t{}\t{}\t{:.1}%\t{}\t{}\t{}\t{:.1}%\t{}\t{}",
            row.skill_id,
            row.status,
            row.invocations,
            row.success_percent(),
            row.last_used
                .map_or_else(|| "never".into(), |value| value.to_string()),
            row.tasks_with,
            row.passed_with,
            row.pass_rate_without_percent(),
            row.declared_effect_methods,
            row.estimated_round_trips_saved,
        );
    }
    let invocations = rows.iter().map(|row| row.invocations).sum::<u64>();
    let saved = rows
        .iter()
        .map(|row| row.estimated_round_trips_saved)
        .sum::<u64>();
    println!("total\t-\t{invocations}\t-\t-\t-\t-\t-\t-\t{saved}");
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
    purge_id: Option<&str>,
    compact: bool,
    feedback: Option<FeedbackOperation<'_>>,
    library: Option<LibraryOperation<'_>>,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    if let Some(operation) = library {
        return run_library_operation(operation, paths, embedding);
    }
    if let Some(skill_id) = purge_id {
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
        println!(
            "Learned skill purged: id={skill_id} generation={generation} removal_only={}",
            publication.removal_only
        );
        return Ok(());
    }

    if compact {
        let now = current_timestamp().context("failed to resolve compaction timestamp")?;
        let cutoff = now.saturating_sub(DEFAULT_RAW_RETENTION_SECONDS);
        let mut store = SkillStore::open_at(paths).context("failed to open learned-skill store")?;
        let report = RetentionService::new(&mut store)
            .compact_before(cutoff, 1, now)
            .context("learned-skill telemetry compaction failed")?;
        println!(
            "Learned-skill telemetry compacted: events={} through_event_id={}",
            report.compacted_events, report.through_event_id
        );
        return Ok(());
    }

    if let Some(feedback) = feedback {
        submit_feedback(feedback, paths, embedding)?;
    }
    Ok(())
}

fn run_library_operation(
    operation: LibraryOperation<'_>,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    match operation {
        LibraryOperation::Import(path) => import_path(path, paths, embedding),
        LibraryOperation::InstallSeeds => {
            let packages = SEED_PACKAGES
                .into_iter()
                .map(|(name, source)| {
                    let package: LearnedSkillPackage = serde_json::from_str(source)
                        .with_context(|| format!("bundled learned-skill seed {name} is invalid"))?;
                    validate_package(&package, name)?;
                    Ok((name, package))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            for (name, package) in packages {
                import_package(package, paths, embedding, name)?;
            }
            Ok(())
        }
        LibraryOperation::Approve(id) => review_proposal(id, true, paths, embedding),
        LibraryOperation::Reject(id) => review_proposal(id, false, paths, embedding),
        LibraryOperation::Activate(id) => activate_skill(id, paths, embedding),
        LibraryOperation::Promote(id) => promote_replacement_skill(id, paths, embedding),
    }
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
        return import_package(package, paths, embedding, &label);
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
        }
    }
    packages.sort();
    if packages.is_empty() || packages.len() > MAX_DIRECTORY_PACKAGES {
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
        import_package(package, paths, embedding, &label)?;
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
    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to open learned-skill package {}", path.display()))?;
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

fn import_package(
    package: LearnedSkillPackage,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
    label: &str,
) -> anyhow::Result<()> {
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
    for suite in held_out_suites {
        suite
            .import(&mut store, &admin, now)
            .context("failed to import learned-skill held-out baseline")?;
    }
    let queued = store
        .enqueue_proposal(&artifact, predecessor_id.as_deref(), now)
        .context("failed to enqueue learned-skill proposal")?;
    drop(store);

    let mut evaluator = AdmissionEvaluator::new(
        SkillStore::open_at(paths)?,
        Embedder::from_config(embedding)?,
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
    for _ in 0..MAX_DIRECTORY_PACKAGES {
        let current = SkillStore::open_at(paths)?
            .get_proposal(&queued.proposal_id)?
            .context("imported proposal disappeared")?;
        if matches!(
            current.status,
            ProposalStatus::AwaitingApproval
                | ProposalStatus::Rejected
                | ProposalStatus::Deferred
                | ProposalStatus::Verified
                | ProposalStatus::Approved
        ) {
            if matches!(
                current.status,
                ProposalStatus::AwaitingApproval | ProposalStatus::Approved
            ) {
                println!(
                    "Learned skill imported: id={} status={}",
                    current.skill_id,
                    proposal_status(current.status)
                );
                return Ok(());
            }
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
        match evaluator.evaluate_next(current_timestamp()?) {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error) => tracing::warn!(error = %error, "learned-skill evaluation will retry"),
        }
    }
    let current = SkillStore::open_at(paths)?
        .get_proposal(&queued.proposal_id)?
        .context("imported proposal disappeared")?;
    anyhow::bail!(
        "learned-skill verification did not complete within the bounded import attempt: id={} status={}",
        current.skill_id,
        proposal_status(current.status)
    )
}

struct LocalOwnerReviewer {
    approve: bool,
    now: i64,
}

impl HumanReviewer for LocalOwnerReviewer {
    fn review(&self, _packet: &super::admission::ReviewPacket) -> ReviewDecision {
        if self.approve {
            ReviewDecision::Approve(AuthenticatedHumanDecision::local_owner(self.now))
        } else {
            ReviewDecision::Deny {
                reason_code: "local_owner_rejected".to_string(),
            }
        }
    }
}

fn review_proposal(
    proposal_id: &str,
    approve: bool,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    let now = current_timestamp().context("failed to resolve review timestamp")?;
    let mut evaluator = AdmissionEvaluator::new(
        SkillStore::open_at(paths)?,
        Embedder::from_config(embedding)?,
        format!("local-review-{}", uuid::Uuid::new_v4()),
    )?;
    let outcome = evaluator
        .review_and_admit(proposal_id, &LocalOwnerReviewer { approve, now }, now)
        .context("learned-skill review failed")?;
    match outcome {
        ReviewOutcome::Canary(result) => {
            // Admission advances the durable desired generation. The operator
            // command is not complete until that generation is published.
            drop(evaluator);
            let coordinator =
                IndexCoordinator::open(paths, Arc::new(Embedder::from_config(embedding)?))?;
            coordinator
                .rebuild_and_publish()
                .context("failed to publish approved learned-skill canary")?;
            println!(
                "Learned skill approved as canary: id={} generation={}",
                result.skill_id, result.generation
            );
        }
        ReviewOutcome::Denied => println!("Learned skill rejected: id={proposal_id}"),
        ReviewOutcome::Cancelled | ReviewOutcome::TimedOut => {
            anyhow::bail!("local-owner learned-skill review did not complete")
        }
    }
    Ok(())
}

fn activate_skill(
    skill_id: &str,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
    let now = current_timestamp().context("failed to resolve activation timestamp")?;
    let embedder = Arc::new(Embedder::from_config(embedding)?);
    let coordinator = IndexCoordinator::open(paths, embedder)?;
    coordinator
        .rebuild_and_publish()
        .context("failed to reconcile learned-skill index before activation")?;
    let mut store = SkillStore::open_at(paths)?;
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
            println!("Learned skill already active: id={skill_id}");
            return Ok(());
        }
        "canary" => {}
        status => anyhow::bail!(
            "learned-skill activation requires an approved canary; current status is {status}"
        ),
    }
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
    println!(
        "Learned skill activated: id={} generation={} removal_only={}",
        skill_id, outcome.desired_generation, publication.removal_only
    );
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
            println!("Learned skill replacement already promoted: id={skill_id} status=active");
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
    println!(
        "Learned skill replacement promoted: id={skill_id} status={} predecessor={predecessor_id} \
         predecessor_status={} generation={} removal_only={}",
        outcome.candidate_status,
        outcome.predecessor_status,
        outcome.desired_generation,
        publication.removal_only
    );
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

    if kind == FeedbackKind::Severe {
        let metadata = store
            .metadata(operation.skill_id)
            .context("failed to inspect feedback target")?
            .context("feedback target disappeared")?;
        let status = LifecycleStatus::from_token(&metadata.status)
            .context("feedback target has an invalid lifecycle status")?;
        if status == LifecycleStatus::Canary || status == LifecycleStatus::Active {
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
            QuarantineExecutor::new(&coordinator)
                .apply(
                    &QuarantinePolicy::conservative("phase5-quarantine-v1"),
                    &evidence,
                    status,
                    row_version,
                    desired_generation,
                    now,
                )
                .context("severe feedback was stored but quarantine failed")?;
        }
    }
    println!("Learned-skill feedback recorded: id={feedback_id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
    use crate::paths::{PathEnvironment, PathPlatform};

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

        run(Some(&artifact.id), false, None, None, &paths, None).unwrap();

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
        for (evidence_id, turn_id, passed, linked) in [
            ("with-pass", "with-1", 1, true),
            ("with-fail", "with-2", 0, true),
            ("without-pass", "without-1", 1, false),
            ("without-fail", "without-2", 0, false),
        ] {
            store
                .conn_mut()
                .execute(
                    "INSERT INTO skill_task_outcomes (
                         evidence_id, turn_id, verify_passed, attempt, source_kind,
                         source_id, production, created_at
                     ) VALUES (?, ?, ?, 1, 'verify_command', 'same-command', 0, 42)",
                    rusqlite::params![evidence_id, turn_id, passed],
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
        assert_eq!(rows[0].success_percent(), 75.0);
        assert_eq!(rows[0].tasks_with, 2);
        assert_eq!(rows[0].passed_with, 1);
        assert_eq!(rows[0].pass_rate_without_percent(), 50.0);
        assert_eq!(rows[0].estimated_round_trips_saved, 0);

        assert_eq!(estimate_round_trips_saved(6, 3), 12);
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

    #[test]
    fn operator_episode_preserves_generation_and_lifecycle_invariants_after_every_step() {
        let (root, paths, _) = fixture();
        let package_path = root.join("episode-seed.json");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&package_path, SEED_PACKAGES[0].1).unwrap();
        let package: LearnedSkillPackage = serde_json::from_str(SEED_PACKAGES[0].1).unwrap();
        let artifact = JsProposal::try_from(package.proposal)
            .unwrap()
            .validate_and_canonicalize()
            .unwrap();

        let assert_state = |expected: &str| {
            let store = SkillStore::open_at(&paths).unwrap();
            assert_eq!(
                store.metadata(&artifact.id).unwrap().unwrap().status,
                expected
            );
            let generation = store.generation_state().unwrap();
            assert_eq!(
                generation.desired_generation, generation.applied_generation,
                "published index must match durable desired generation after {expected}"
            );
            assert!(artifact.verify_identity().is_ok());
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
            Some(LibraryOperation::Approve(&artifact.id)),
            &paths,
            None,
        )
        .unwrap();
        assert_state("canary");

        run(
            None,
            false,
            None,
            Some(LibraryOperation::Activate(&artifact.id)),
            &paths,
            None,
        )
        .unwrap();
        assert_state("active");

        run(
            None,
            false,
            Some(FeedbackOperation {
                skill_id: &artifact.id,
                invocation_id: None,
                kind: "severe",
                reason_code: "integrity",
                idempotency_key: "gym-episode-severe",
            }),
            None,
            &paths,
            None,
        )
        .unwrap();
        assert_state("quarantined");

        run(Some(&artifact.id), false, None, None, &paths, None).unwrap();
        let store = SkillStore::open_at(&paths).unwrap();
        assert!(store.metadata(&artifact.id).unwrap().is_none());
        let generation = store.generation_state().unwrap();
        assert_eq!(generation.desired_generation, generation.applied_generation);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
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

        review_proposal(&skill_id, false, &paths, None).unwrap();
        let store = SkillStore::open_at(&paths).unwrap();
        assert!(
            load_proposal_queue(&store)
                .unwrap()
                .iter()
                .all(|row| row.skill_id != skill_id),
            "a rejected proposal is terminal and must not be listed"
        );
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
        review_proposal(&skill_id, true, &paths, None).unwrap();
        assert_eq!(
            SkillStore::open_at(&paths)
                .unwrap()
                .revision_status(&skill_id)
                .unwrap()
                .as_deref(),
            Some("canary")
        );
        activate_skill(&skill_id, &paths, None).unwrap();
        let store = SkillStore::open_at(&paths).unwrap();
        assert_eq!(
            store.revision_status(&skill_id).unwrap().as_deref(),
            Some("active")
        );
        assert!(store.is_retrievable(&skill_id).unwrap());
        drop(store);
        activate_skill(&skill_id, &paths, None).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
