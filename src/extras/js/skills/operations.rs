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
use super::lifecycle::{CoordinatedLifecycle, EvidenceSnapshot, HumanApproval, LifecycleService};
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
        ReviewOutcome::Canary(result) => println!(
            "Learned skill approved as canary: id={} generation={}",
            result.skill_id, result.generation
        ),
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
        anyhow::bail!("replacement activation requires the evidence-based promotion surface");
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
        vec![report_id],
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

fn proposal_status(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Evaluating => "evaluating",
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
