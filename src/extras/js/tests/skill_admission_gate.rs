use crate::extras::js::skills::admission::{
    AdmissionError, AdmissionEvaluator, AuthenticatedHumanDecision, HumanReviewer, ReviewDecision,
    ReviewOutcome, ReviewPacket,
};
use crate::extras::js::skills::embed::{Embedder, EmbeddingBackend, EmbeddingError};
use crate::extras::js::skills::held_out::{
    ExpectedJsValue, HeldOutCase, HeldOutSelector, HeldOutSuiteDraft, TranscriptExpectation,
};
use crate::extras::js::skills::store::{AdminIdentity, ProposalStatus, SkillStore};
use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn paths() -> (PathBuf, AppPaths) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().join(format!(
        "admission_gate_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let environment = PathEnvironment {
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
    };
    (root, AppPaths::resolve(&environment).expect("paths"))
}

fn artifact() -> SkillArtifact {
    SkillArtifact::new(
        "function normalize(v) { return String(v).trim(); }".to_string(),
        "Normalize a value.".to_string(),
        vec!["normalize".to_string()],
        vec![SkillExport {
            name: "normalize".to_string(),
            signature: "normalize(value: unknown): string".to_string(),
        }],
        vec!["normalize(' x ') === 'x'".to_string()],
        CapabilityManifest::pure(),
    )
    .expect("artifact")
}

fn suite() -> HeldOutSuiteDraft {
    HeldOutSuiteDraft {
        selector: HeldOutSelector {
            tags: vec!["normalize".to_string()],
            exports: vec!["normalize".to_string()],
            capability_tier: Some("pure".to_string()),
        },
        cases: vec![HeldOutCase {
            expression: "normalize('\\tvalue\\n')".to_string(),
            expected: ExpectedJsValue::String("value".to_string()),
            fake_files: BTreeMap::new(),
            transcript: TranscriptExpectation::default(),
        }],
    }
}

fn evaluator(with_suite: bool) -> (PathBuf, AppPaths, AdmissionEvaluator, SkillArtifact) {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    if with_suite {
        suite()
            .import(
                &mut store,
                &AdminIdentity::authenticated("suite-admin").unwrap(),
                5,
            )
            .expect("suite");
    }
    let artifact = artifact();
    store
        .enqueue_proposal(&artifact, None, 10)
        .expect("proposal");
    let evaluator =
        AdmissionEvaluator::new(store, Embedder::new().unwrap(), "worker-1").expect("evaluator");
    (root, paths, evaluator, artifact)
}

struct Approver {
    now: i64,
    packet: Mutex<Option<ReviewPacket>>,
}

impl HumanReviewer for Approver {
    fn review(&self, packet: &ReviewPacket) -> ReviewDecision {
        *self.packet.lock().unwrap() = Some(packet.clone());
        ReviewDecision::Approve(AuthenticatedHumanDecision::verified(
            "decision-1",
            "human-reviewer",
            self.now,
        ))
    }
}

struct Denier;

impl HumanReviewer for Denier {
    fn review(&self, _packet: &ReviewPacket) -> ReviewDecision {
        ReviewDecision::Deny {
            reason_code: "human_denied".to_string(),
        }
    }
}

struct Cancelled;

impl HumanReviewer for Cancelled {
    fn review(&self, _packet: &ReviewPacket) -> ReviewDecision {
        ReviewDecision::Cancelled
    }
}

#[test]
fn skill_admission_gate_evaluates_then_human_approves_exactly_one_canary() {
    let (root, _paths, mut evaluator, artifact) = evaluator(true);
    let report = evaluator
        .evaluate_next(20)
        .expect("evaluation")
        .expect("report");
    assert_eq!(report.outcome, "passed");
    let proposal = evaluator
        .store()
        .get_proposal(&artifact.id)
        .unwrap()
        .unwrap();
    assert_eq!(proposal.status, ProposalStatus::AwaitingApproval);

    let approver = Approver {
        now: 21,
        packet: Mutex::new(None),
    };
    let outcome = evaluator
        .review_and_admit(&artifact.id, &approver, 21)
        .expect("approval");
    let ReviewOutcome::Canary(result) = outcome else {
        panic!("expected canary");
    };
    assert_eq!(result.skill_id, artifact.id);
    assert_eq!(result.generation, 1);
    assert!(!result.idempotent);
    assert_eq!(
        evaluator.store().revision_status(&artifact.id).unwrap(),
        Some("canary".to_string())
    );
    assert!(evaluator.store().list_retrievable().unwrap().is_empty());
    assert_eq!(evaluator.store().desired_generation().unwrap(), 1);
    let fts_rows: i64 = evaluator
        .store()
        .conn()
        .query_row("SELECT COUNT(*) FROM skill_search", [], |row| row.get(0))
        .unwrap();
    assert_eq!(fts_rows, 0, "canary must not enter FTS retrieval");

    let packet = approver.packet.lock().unwrap().clone().unwrap();
    assert_eq!(packet.source, artifact.source);
    assert_eq!(packet.report_id, report.report_id);
    assert!(!format!("{packet:?}").contains("function normalize"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_concurrency_duplicate_approval_is_idempotent() {
    let (root, _paths, mut evaluator, artifact) = evaluator(true);
    evaluator.evaluate_next(20).unwrap().unwrap();
    let approver = Approver {
        now: 21,
        packet: Mutex::new(None),
    };
    evaluator
        .review_and_admit(&artifact.id, &approver, 21)
        .expect("first approval");
    let second = evaluator
        .review_and_admit(&artifact.id, &approver, 22)
        .expect("idempotent retry");
    let ReviewOutcome::Canary(second) = second else {
        panic!("expected canary");
    };
    assert!(second.idempotent);
    assert_eq!(second.generation, 1);
    assert_eq!(evaluator.store().desired_generation().unwrap(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_review_deny_cancel_and_timeout_never_create_canary() {
    let (root, _paths, mut denied, denied_artifact) = evaluator(true);
    denied.evaluate_next(20).unwrap().unwrap();
    assert_eq!(
        denied
            .review_and_admit(&denied_artifact.id, &Denier, 21)
            .unwrap(),
        ReviewOutcome::Denied
    );
    assert_eq!(
        denied.store().revision_status(&denied_artifact.id).unwrap(),
        Some("rejected".to_string())
    );
    assert_eq!(denied.store().desired_generation().unwrap(), 0);

    let (cancelled_root, _other_paths, mut cancelled, cancelled_artifact) = evaluator(true);
    cancelled.evaluate_next(20).unwrap().unwrap();
    assert_eq!(
        cancelled
            .review_and_admit(&cancelled_artifact.id, &Cancelled, 21)
            .unwrap(),
        ReviewOutcome::Cancelled
    );
    assert_eq!(
        cancelled
            .store()
            .revision_status(&cancelled_artifact.id)
            .unwrap(),
        Some("verified".to_string())
    );
    assert_eq!(cancelled.store().desired_generation().unwrap(), 0);

    struct TimedOut;
    impl HumanReviewer for TimedOut {
        fn review(&self, _packet: &ReviewPacket) -> ReviewDecision {
            ReviewDecision::TimedOut
        }
    }
    assert_eq!(
        cancelled
            .review_and_admit(&cancelled_artifact.id, &TimedOut, 22)
            .unwrap(),
        ReviewOutcome::TimedOut
    );
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(cancelled_root);
}

struct TamperingApprover {
    paths: AppPaths,
    now: i64,
}

impl HumanReviewer for TamperingApprover {
    fn review(&self, packet: &ReviewPacket) -> ReviewDecision {
        let mut other = SkillStore::open_at(&self.paths).expect("second connection");
        other
            .conn_mut()
            .execute(
                "UPDATE skill_revisions SET row_version = row_version + 1 WHERE id = ?1",
                [&packet.artifact_id],
            )
            .expect("tamper version");
        ReviewDecision::Approve(AuthenticatedHumanDecision::verified(
            "decision-stale",
            "human-reviewer",
            self.now,
        ))
    }
}

struct SuiteDisablingApprover {
    paths: AppPaths,
    now: i64,
}

impl HumanReviewer for SuiteDisablingApprover {
    fn review(&self, _packet: &ReviewPacket) -> ReviewDecision {
        let store = SkillStore::open_at(&self.paths).expect("second connection");
        store
            .conn()
            .execute("UPDATE held_out_suites SET enabled = 0", [])
            .expect("disable suite");
        ReviewDecision::Approve(AuthenticatedHumanDecision::verified(
            "decision-stale-suite",
            "human-reviewer",
            self.now,
        ))
    }
}

#[test]
fn skill_admission_transaction_failures_and_review_staleness_roll_back() {
    let (root, paths, mut first_evaluator, artifact) = evaluator(true);
    first_evaluator.evaluate_next(20).unwrap().unwrap();
    let error = first_evaluator
        .review_and_admit(&artifact.id, &TamperingApprover { paths, now: 21 }, 21)
        .expect_err("stale review must fail");
    assert!(matches!(error, AdmissionError::StaleReview));
    assert_ne!(
        first_evaluator
            .store()
            .revision_status(&artifact.id)
            .unwrap(),
        Some("canary".to_string())
    );
    assert_eq!(first_evaluator.store().desired_generation().unwrap(), 0);
    let approvals: i64 = first_evaluator
        .store()
        .conn()
        .query_row("SELECT COUNT(*) FROM skill_approvals", [], |row| row.get(0))
        .unwrap();
    assert_eq!(approvals, 0);
    let _ = std::fs::remove_dir_all(root);

    let (root, paths, mut suite_evaluator, artifact) = evaluator(true);
    suite_evaluator.evaluate_next(20).unwrap().unwrap();
    let error = suite_evaluator
        .review_and_admit(&artifact.id, &SuiteDisablingApprover { paths, now: 21 }, 21)
        .expect_err("changed held-out suite selection must fail");
    assert!(matches!(error, AdmissionError::StaleReview));
    assert_ne!(
        suite_evaluator
            .store()
            .revision_status(&artifact.id)
            .unwrap(),
        Some("canary".to_string())
    );
    assert_eq!(suite_evaluator.store().desired_generation().unwrap(), 0);
    let _ = std::fs::remove_dir_all(root);

    let (root, _paths, mut evaluator, artifact) = evaluator(true);
    evaluator.evaluate_next(20).unwrap().unwrap();
    let report_id = evaluator
        .store()
        .get_proposal(&artifact.id)
        .unwrap()
        .unwrap()
        .report_id
        .unwrap();
    evaluator
        .store_mut()
        .conn_mut()
        .execute(
            "UPDATE evaluation_reports SET summary_json = '{}' WHERE report_id = ?1",
            [&report_id],
        )
        .expect("tamper report");
    assert!(matches!(
        evaluator.review_and_admit(&artifact.id, &Cancelled, 21),
        Err(AdmissionError::Store(
            crate::extras::js::skills::store::StoreError::CorruptRow(_)
        ))
    ));
    assert_ne!(
        evaluator.store().revision_status(&artifact.id).unwrap(),
        Some("canary".to_string())
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_store_pending_lifecycle_missing_suite_is_verified_but_not_approvable() {
    let (root, _paths, mut evaluator, artifact) = evaluator(false);
    let report = evaluator.evaluate_next(20).unwrap().unwrap();
    assert_eq!(
        report.reason_code.as_deref(),
        Some("held_out_suite_required")
    );
    let proposal = evaluator
        .store()
        .get_proposal(&artifact.id)
        .unwrap()
        .unwrap();
    assert_eq!(proposal.status, ProposalStatus::Verified);
    assert!(matches!(
        evaluator.review_and_admit(&artifact.id, &Cancelled, 21),
        Err(AdmissionError::NotAwaitingApproval)
    ));
    assert_eq!(evaluator.store().desired_generation().unwrap(), 0);

    let admin = AdminIdentity::authenticated("suite-admin").unwrap();
    suite()
        .import(evaluator.store_mut(), &admin, 22)
        .expect("trusted suite");
    evaluator
        .request_reevaluation(&artifact.id, &admin, 23)
        .expect("authenticated reevaluation");
    let report = evaluator.evaluate_next(24).unwrap().unwrap();
    assert_eq!(report.outcome, "passed");
    assert_eq!(
        evaluator
            .store()
            .get_proposal(&artifact.id)
            .unwrap()
            .unwrap()
            .status,
        ProposalStatus::AwaitingApproval
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_api_visibility_has_no_active_transition() {
    let source = include_str!("../skills/admission_store.rs");
    assert!(!source.contains("status = 'active'"));
    assert!(!source.contains("pub fn approve_canary"));
}

struct UnavailableEmbedding;

impl EmbeddingBackend for UnavailableEmbedding {
    fn embed_documents(&self, _documents: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Err(EmbeddingError::RequestFailed("injected outage".to_string()))
    }

    fn embed_query(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::RequestFailed("injected outage".to_string()))
    }

    fn model_id(&self) -> &str {
        "unavailable"
    }

    fn model_revision(&self) -> &str {
        "v1"
    }

    fn dimensions(&self) -> usize {
        8
    }

    fn normalized(&self) -> bool {
        true
    }
}

#[test]
fn skill_admission_gate_retry_budget_ends_in_stable_rejection() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    suite()
        .import(
            &mut store,
            &AdminIdentity::authenticated("suite-admin").unwrap(),
            5,
        )
        .expect("suite");
    let artifact = artifact();
    store
        .enqueue_proposal(&artifact, None, 10)
        .expect("proposal");
    let embedder = Embedder::with_backend(Arc::new(UnavailableEmbedding)).expect("embedder");
    let mut evaluator =
        AdmissionEvaluator::new(store, embedder, "retry-worker").expect("evaluator");

    for attempt in 1..8 {
        let error = evaluator
            .evaluate_next(i64::from(attempt) * 1_000)
            .expect_err("retryable outage");
        assert!(matches!(error, AdmissionError::Retryable(_)));
    }
    let report = evaluator
        .evaluate_next(8_000)
        .expect("final attempt")
        .expect("rejection report");
    assert_eq!(report.outcome, "rejected");
    assert_eq!(report.reason_code.as_deref(), Some("embedding_unavailable"));
    assert_eq!(
        evaluator.store().revision_status(&artifact.id).unwrap(),
        Some("rejected".to_string())
    );
    let _ = std::fs::remove_dir_all(root);
}
