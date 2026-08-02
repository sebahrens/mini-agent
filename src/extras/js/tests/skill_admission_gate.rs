use crate::extras::js::skills::admission::{
    AdmissionError, AdmissionEvaluator, AuthenticatedHumanDecision, HumanReviewer, ReviewDecision,
    ReviewOutcome, ReviewPacket,
};
use crate::extras::js::skills::embed::{Embedder, EmbeddingBackend, EmbeddingError};
use crate::extras::js::skills::held_out::{
    ExpectedJsValue, HeldOutCase, HeldOutSelector, HeldOutSuiteDraft, TranscriptExpectation,
};
use crate::extras::js::skills::lifecycle::{
    EvidenceSnapshot, HumanApproval, LifecycleError, LifecycleService,
};
use crate::extras::js::skills::store::{AdminIdentity, ProposalStatus, SkillStore};
use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Barrier;
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

fn authorization_fixture() -> (
    PathBuf,
    AppPaths,
    AdmissionEvaluator,
    SkillArtifact,
    String,
    u64,
    u64,
) {
    let (root, paths, mut evaluator, artifact) = evaluator(true);
    let report = evaluator.evaluate_next(20).unwrap().unwrap();
    let proposal = evaluator
        .store()
        .get_proposal(&artifact.id)
        .unwrap()
        .unwrap();
    let artifact_version = evaluator
        .store()
        .revision_row_version(&artifact.id)
        .unwrap()
        .unwrap();
    (
        root,
        paths,
        evaluator,
        artifact,
        report.report_id,
        artifact_version,
        proposal.row_version,
    )
}

#[test]
fn authenticated_approval_authorization_rejects_untrusted_shape_and_expiry() {
    let (root, _paths, mut evaluator, artifact, report_id, _, _) = authorization_fixture();
    assert!(matches!(
        evaluator.authorize_canary_for_test("bad-principal", "", &artifact, &report_id, 21, 22),
        Err(AdmissionError::Store(
            crate::extras::js::skills::store::StoreError::Unauthorized
        ))
    ));
    assert!(matches!(
        evaluator.authorize_canary_for_test("bad-time", "reviewer", &artifact, &report_id, -1, 22),
        Err(AdmissionError::Store(
            crate::extras::js::skills::store::StoreError::Unauthorized
        ))
    ));
    let authorization = evaluator
        .authorize_canary_for_test(
            "expires-on-boundary",
            "reviewer",
            &artifact,
            &report_id,
            21,
            22,
        )
        .unwrap();
    let proposal = evaluator
        .store()
        .get_proposal(&artifact.id)
        .unwrap()
        .unwrap();
    let artifact_version = evaluator
        .store()
        .revision_row_version(&artifact.id)
        .unwrap()
        .unwrap();
    assert!(matches!(
        evaluator.consume_canary_for_test(
            &proposal.proposal_id,
            &artifact.id,
            &report_id,
            artifact_version,
            proposal.row_version,
            &authorization,
            22,
        ),
        Err(AdmissionError::Store(
            crate::extras::js::skills::store::StoreError::Unauthorized
        ))
    ));
    let consumed: Option<i64> = evaluator
        .store()
        .conn()
        .query_row(
            "SELECT consumed_at FROM skill_approval_authorizations WHERE authorization_id = ?",
            ["expires-on-boundary"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(consumed, None);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn authenticated_approval_authorization_rejects_tampered_exact_binding() {
    let (root, _paths, mut evaluator, artifact, report_id, artifact_version, proposal_version) =
        authorization_fixture();
    let authorization = evaluator
        .authorize_canary_for_test("exact-binding", "reviewer", &artifact, &report_id, 21, 30)
        .unwrap();
    let other = SkillArtifact::new(
        "function run() { return 2; }".to_string(),
        "Other authorization artifact".to_string(),
        vec!["authorization".to_string()],
        vec![SkillExport {
            name: "run".to_string(),
            signature: "() => number".to_string(),
        }],
        vec!["run() === 2".to_string()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    evaluator.store_mut().insert_verified(&other).unwrap();
    let original: (String, String, String, String) = evaluator
        .store()
        .conn()
        .query_row(
            "SELECT artifact_id, report_id, manifest_digest, transition
               FROM skill_approval_authorizations WHERE authorization_id = ?",
            ["exact-binding"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();

    for (column, bad_value) in [
        ("artifact_id", other.id.clone()),
        ("report_id", "wrong-report".to_string()),
        ("manifest_digest", "0".repeat(64)),
        ("transition", "canary_to_active".to_string()),
    ] {
        evaluator
            .store_mut()
            .conn_mut()
            .execute(
                &format!(
                    "UPDATE skill_approval_authorizations SET {column} = ?1
                      WHERE authorization_id = 'exact-binding'"
                ),
                [bad_value],
            )
            .unwrap();
        assert!(matches!(
            evaluator.consume_canary_for_test(
                &artifact.id,
                &artifact.id,
                &report_id,
                artifact_version,
                proposal_version,
                &authorization,
                22,
            ),
            Err(AdmissionError::Store(
                crate::extras::js::skills::store::StoreError::Unauthorized
            ))
        ));
        let replacement = match column {
            "artifact_id" => &original.0,
            "report_id" => &original.1,
            "manifest_digest" => &original.2,
            _ => &original.3,
        };
        evaluator
            .store_mut()
            .conn_mut()
            .execute(
                &format!(
                    "UPDATE skill_approval_authorizations SET {column} = ?1
                      WHERE authorization_id = 'exact-binding'"
                ),
                [replacement],
            )
            .unwrap();
    }
    let result = evaluator
        .consume_canary_for_test(
            &artifact.id,
            &artifact.id,
            &report_id,
            artifact_version,
            proposal_version,
            &authorization,
            22,
        )
        .unwrap();
    assert!(!result.idempotent);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn authenticated_approval_authorization_stale_and_failed_transaction_preserve_token() {
    let (root, _paths, mut evaluator, artifact, report_id, artifact_version, proposal_version) =
        authorization_fixture();
    let authorization = evaluator
        .authorize_canary_for_test("rollback-token", "reviewer", &artifact, &report_id, 21, 30)
        .unwrap();
    assert!(
        evaluator
            .consume_canary_for_test(
                &artifact.id,
                &artifact.id,
                &report_id,
                artifact_version + 1,
                proposal_version,
                &authorization,
                22,
            )
            .is_err()
    );
    evaluator
        .store_mut()
        .conn_mut()
        .execute(
            "UPDATE skill_generations SET desired_generation = ?",
            [i64::MAX],
        )
        .unwrap();
    assert!(
        evaluator
            .consume_canary_for_test(
                &artifact.id,
                &artifact.id,
                &report_id,
                artifact_version,
                proposal_version,
                &authorization,
                22,
            )
            .is_err()
    );
    let (consumed, status): (Option<i64>, String) = evaluator
        .store()
        .conn()
        .query_row(
            "SELECT a.consumed_at, r.status
               FROM skill_approval_authorizations a
               JOIN skill_revisions r ON r.id = a.artifact_id
              WHERE a.authorization_id = ?",
            ["rollback-token"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(consumed, None);
    assert_eq!(status, "verified");
    evaluator
        .store_mut()
        .conn_mut()
        .execute("UPDATE skill_generations SET desired_generation = 0", [])
        .unwrap();
    let retry = evaluator
        .authorize_canary_for_test("rollback-token", "reviewer", &artifact, &report_id, 23, 30)
        .expect("the exact unconsumed authorization survives rollback");
    evaluator
        .consume_canary_for_test(
            &artifact.id,
            &artifact.id,
            &report_id,
            artifact_version,
            proposal_version,
            &retry,
            23,
        )
        .unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn authenticated_approval_authorization_two_connections_consume_exactly_once() {
    let (root, paths, mut evaluator, artifact, report_id, artifact_version, proposal_version) =
        authorization_fixture();
    let authorization = evaluator
        .authorize_canary_for_test(
            "concurrent-one-use",
            "reviewer",
            &artifact,
            &report_id,
            21,
            30,
        )
        .unwrap();
    drop(evaluator);

    let barrier = Arc::new(Barrier::new(2));
    let mut joins = Vec::new();
    for worker in ["concurrent-a", "concurrent-b"] {
        let paths = paths.clone();
        let artifact_id = artifact.id.clone();
        let report_id = report_id.clone();
        let authorization = authorization.clone();
        let barrier = Arc::clone(&barrier);
        joins.push(std::thread::spawn(move || {
            let store = SkillStore::open_at(&paths).unwrap();
            let mut evaluator =
                AdmissionEvaluator::new(store, Embedder::new().unwrap(), worker).unwrap();
            barrier.wait();
            evaluator.consume_canary_for_test(
                &artifact_id,
                &artifact_id,
                &report_id,
                artifact_version,
                proposal_version,
                &authorization,
                22,
            )
        }));
    }
    let outcomes = joins
        .into_iter()
        .map(|join| join.join().expect("consumer thread"))
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.is_err()).count(),
        1
    );

    let mut store = SkillStore::open_at(&paths).unwrap();
    let (consumed, approvals): (i64, i64) = store
        .conn_mut()
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM skill_approval_authorizations
                  WHERE authorization_id = 'concurrent-one-use' AND consumed_at IS NOT NULL),
                (SELECT COUNT(*) FROM skill_approvals WHERE skill_id = ?1)",
            [&artifact.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((consumed, approvals), (1, 1));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn authenticated_approval_authorization_cannot_cross_transition_or_replay() {
    let (root, _paths, mut evaluator, artifact, report_id, artifact_version, proposal_version) =
        authorization_fixture();
    let authorization = evaluator
        .authorize_canary_for_test("one-use", "reviewer", &artifact, &report_id, 21, 30)
        .unwrap();
    evaluator
        .consume_canary_for_test(
            &artifact.id,
            &artifact.id,
            &report_id,
            artifact_version,
            proposal_version,
            &authorization,
            22,
        )
        .unwrap();
    assert!(matches!(
        evaluator.authorize_canary_for_test("one-use", "reviewer", &artifact, &report_id, 21, 30,),
        Err(AdmissionError::Store(
            crate::extras::js::skills::store::StoreError::Unauthorized
        ))
    ));

    let row_version = evaluator
        .store()
        .revision_row_version(&artifact.id)
        .unwrap()
        .unwrap() as i64;
    let mut lifecycle = LifecycleService::new(evaluator.store_mut());
    lifecycle
        .register_policy("authorization-v1", "{}", 22)
        .unwrap();
    let snapshot = EvidenceSnapshot::new(
        artifact.id.clone(),
        None,
        "authorization-v1",
        vec![],
        BTreeMap::new(),
        row_version,
        None,
        1,
    )
    .unwrap();
    let reused =
        HumanApproval::verified("distinct-root-review", "reviewer", report_id, row_version)
            .unwrap();
    assert!(matches!(
        lifecycle.activate_root(
            "cross-transition",
            &artifact.id,
            &reused,
            &authorization,
            &snapshot,
            23,
        ),
        Err(LifecycleError::Store(
            crate::extras::js::skills::store::StoreError::Unauthorized
        ))
    ));
    let _ = std::fs::remove_dir_all(root);
}
