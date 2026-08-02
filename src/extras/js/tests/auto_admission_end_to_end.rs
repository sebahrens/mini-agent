use crate::extras::js::host::AllowConfig;
use crate::extras::js::skills::admission::{
    AdmissionEvaluator, AdmissionWorker, AuthenticatedHumanDecision, HumanReviewer, ReviewDecision,
    ReviewOutcome, ReviewPacket,
};
use crate::extras::js::skills::embed::Embedder;
use crate::extras::js::skills::held_out::{
    ExpectedJsValue, HeldOutCase, HeldOutSelector, HeldOutSuiteDraft, TranscriptExpectation,
};
use crate::extras::js::skills::proposal::ProposalQueue;
use crate::extras::js::skills::store::{AdminIdentity, ProposalStatus, SkillStore};
use crate::extras::js::skills::visibility::SkillIndex;
use crate::extras::js::tool::{JsArgs, JsTool};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use crate::sandbox::Sandbox;
use rig::tool::Tool;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn paths() -> (PathBuf, AppPaths) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().join(format!(
        "auto_admission_{}_{}",
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

fn payload(source: &str, test: &str) -> String {
    serde_json::json!({
        "source": source,
        "description": "Normalize a value.",
        "exports": [{"name": "normalize", "signature": "normalize(value: unknown): string"}],
        "tests": [test],
        "capability": {"tier": "pure", "grants": []},
        "tags": ["normalize"]
    })
    .to_string()
}

fn import_suite(store: &mut SkillStore) {
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
    .import(
        store,
        &AdminIdentity::authenticated("trusted-suite-admin").unwrap(),
        5,
    )
    .expect("suite import");
}

struct Approver;

impl HumanReviewer for Approver {
    fn review(&self, packet: &ReviewPacket) -> ReviewDecision {
        assert_eq!(packet.held_out_suite_hashes.len(), 1);
        assert!(!format!("{packet:?}").contains("\\tvalue"));
        ReviewDecision::Approve(AuthenticatedHumanDecision::verified(
            "e2e-decision",
            "authenticated-human",
            30,
        ))
    }
}

#[tokio::test]
async fn auto_admission_end_to_end_proposal_to_non_retrievable_canary() {
    let (root, paths) = paths();
    let mut setup = SkillStore::open_at(&paths).expect("setup store");
    import_suite(&mut setup);
    drop(setup);

    let store = SkillStore::open_at(&paths).expect("host store");
    let worker =
        ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).expect("worker");
    let tool = JsTool::new_with_proposals(
        Sandbox::new(false, "bwrap"),
        None,
        None,
        AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        worker,
    );
    let response = tool
        .call(JsArgs {
            code: format!(
                "propose_skill({})",
                payload(
                    "function normalize(v) { return String(v).trim(); }",
                    "normalize(' x ') === 'x'"
                )
            ),
        })
        .await
        .expect("proposal host");
    let response: serde_json::Value = serde_json::from_str(&response).expect("response");
    let skill_id = response["id"].as_str().unwrap().to_string();
    assert_eq!(response["status"], "pending");
    drop(tool);

    let store = SkillStore::open_at(&paths).expect("evaluator store");
    let evaluator = AdmissionEvaluator::new(store, Embedder::new().unwrap(), "e2e-worker").unwrap();
    let admission_worker = AdmissionWorker::start(evaluator).expect("admission worker");
    let inspector = SkillStore::open_at(&paths).expect("inspector");
    let deadline = Instant::now() + Duration::from_secs(2);
    let report_id = loop {
        let proposal = inspector.get_proposal(&skill_id).unwrap().unwrap();
        if proposal.status == ProposalStatus::AwaitingApproval {
            break proposal.report_id.expect("bound report");
        }
        assert!(
            Instant::now() < deadline,
            "admission worker did not evaluate proposal"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let report = inspector
        .get_evaluation_report(&report_id)
        .unwrap()
        .expect("evaluation report");
    assert_eq!(report.outcome, "passed");
    drop(inspector);
    drop(admission_worker);

    let store = SkillStore::open_at(&paths).expect("review store");
    let mut evaluator =
        AdmissionEvaluator::new(store, Embedder::new().unwrap(), "review-worker").unwrap();
    let outcome = evaluator
        .review_and_admit(&skill_id, &Approver, 30)
        .expect("human approval");
    let ReviewOutcome::Canary(canary) = outcome else {
        panic!("expected canary");
    };
    assert_eq!(canary.generation, 1);
    assert_eq!(
        evaluator.store().revision_status(&skill_id).unwrap(),
        Some("canary".to_string())
    );

    let index = SkillIndex::load(evaluator.store()).expect("active index");
    let manifest = index.manifest();
    let bundle = index.freeze(std::slice::from_ref(&skill_id));
    assert!(!index.contains(&skill_id));
    assert!(!manifest.contains(&skill_id));
    assert!(!bundle.contains(&skill_id));
    assert!(!bundle.js_source().contains("function normalize"));

    let runtime = JsTool::new(
        Sandbox::new(false, "bwrap"),
        None,
        None,
        AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
    );
    assert_eq!(
        runtime
            .call(JsArgs {
                code: "typeof normalize".to_string()
            })
            .await
            .unwrap(),
        "undefined"
    );
    drop(runtime);
    drop(evaluator);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn auto_admission_failure_matrix_rejects_bypasses_and_reproposal_is_terminal() {
    let (root, paths) = paths();
    let mut setup = SkillStore::open_at(&paths).expect("setup store");
    import_suite(&mut setup);
    drop(setup);

    let store = SkillStore::open_at(&paths).expect("host store");
    let worker =
        ProposalQueue::start_store_worker(store, 2, Duration::from_secs(1)).expect("worker");
    let tool = JsTool::new_with_proposals(
        Sandbox::new(false, "bwrap"),
        None,
        None,
        AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        worker,
    );
    let invalid = tool
        .call(JsArgs {
            code: "propose_skill({source: 'secret'})".to_string(),
        })
        .await
        .unwrap();
    assert!(invalid.contains("exports"));

    let failing_payload = payload(
        "function normalize(v) { return String(v); }",
        "normalize(' x ') === 'x'",
    );
    let submitted = tool
        .call(JsArgs {
            code: format!("propose_skill({failing_payload})"),
        })
        .await
        .unwrap();
    let skill_id = serde_json::from_str::<serde_json::Value>(&submitted).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    drop(tool);

    let store = SkillStore::open_at(&paths).expect("evaluator store");
    let mut evaluator =
        AdmissionEvaluator::new(store, Embedder::new().unwrap(), "failure-worker").unwrap();
    let rejected = evaluator.evaluate_next(20).unwrap().unwrap();
    assert_eq!(rejected.outcome, "rejected");
    assert_eq!(
        evaluator.store().revision_status(&skill_id).unwrap(),
        Some("rejected".to_string())
    );
    assert_eq!(evaluator.store().desired_generation().unwrap(), 0);
    assert!(evaluator.store().list_retrievable().unwrap().is_empty());
    drop(evaluator);

    let store = SkillStore::open_at(&paths).expect("reproposal store");
    let worker =
        ProposalQueue::start_store_worker(store, 2, Duration::from_secs(1)).expect("worker");
    let retry_tool = JsTool::new_with_proposals(
        Sandbox::new(false, "bwrap"),
        None,
        None,
        AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        worker,
    );
    let retry = retry_tool
        .call(JsArgs {
            code: format!("propose_skill({failing_payload})"),
        })
        .await
        .unwrap();
    let retry: serde_json::Value = serde_json::from_str(&retry).unwrap();
    assert_eq!(retry["id"], skill_id);
    assert_eq!(retry["status"], "rejected");
    assert_eq!(retry["report_id"], rejected.report_id);
    drop(retry_tool);

    let store = SkillStore::open_at(&paths).expect("final store");
    assert_eq!(
        store.get_proposal(&skill_id).unwrap().unwrap().status,
        ProposalStatus::Rejected
    );
    assert_eq!(store.count_proposals().unwrap(), 1);
    assert_eq!(store.desired_generation().unwrap(), 0);
    let _ = std::fs::remove_dir_all(root);
}
