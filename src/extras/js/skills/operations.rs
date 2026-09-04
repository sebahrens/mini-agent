//! Explicit operator surfaces for learned-skill retention and privacy purge.
//!
//! These commands run before provider initialization. Purge uses the same
//! coordinator transaction/publication gate as lifecycle removals; compaction
//! never deletes raw events until their daily aggregates and watermark commit.

use std::sync::Arc;

use anyhow::Context;

use super::coordinator::IndexCoordinator;
use super::embed::Embedder;
use super::feedback::{
    ActorKind, AuthenticatedActor, FeedbackCommand, FeedbackKind, FeedbackService,
};
use super::lifecycle::LifecycleStatus;
use super::privacy::Redactor;
use super::quarantine::{
    QuarantineEvidence, QuarantineExecutor, QuarantinePolicy, QuarantineReason,
};
use super::retention::{CoordinatedRetention, DEFAULT_RAW_RETENTION_SECONDS, RetentionService};
use super::store::{SkillStore, current_timestamp};
use crate::config::EmbeddingConfig;
use crate::paths::AppPaths;

pub(crate) struct FeedbackOperation<'a> {
    pub(crate) skill_id: &'a str,
    pub(crate) invocation_id: Option<&'a str>,
    pub(crate) kind: &'a str,
    pub(crate) reason_code: &'a str,
    pub(crate) idempotency_key: &'a str,
}

pub(crate) fn run(
    purge_id: Option<&str>,
    compact: bool,
    feedback: Option<FeedbackOperation<'_>>,
    paths: &AppPaths,
    embedding: Option<&EmbeddingConfig>,
) -> anyhow::Result<()> {
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

        run(Some(&artifact.id), false, None, &paths, None).unwrap();

        let store = SkillStore::open_at(&paths).unwrap();
        assert!(store.get(&artifact.id).unwrap().is_none());
        let state = store.generation_state().unwrap();
        assert_eq!(state.desired_generation, state.applied_generation);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_compaction_is_safe_on_an_empty_store() {
        let (root, paths, _artifact) = fixture();
        run(None, true, None, &paths, None).unwrap();
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
}
