use crate::extras::js::host::AllowConfig;
use crate::extras::js::skills::proposal::{
    AttemptBudget, JsCapability, JsCapabilityScope, JsExport, JsProposal, ProposalEffectService,
    ProposalError, ProposalHost, ProposalQueue,
};
use crate::extras::js::skills::store::{EnqueueResult, EnqueueStatus, SkillStore};
use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::extras::js::tool::{JsArgs, JsTool};
use crate::extras::js::types::{EffectServiceError, PermCancellation};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use crate::sandbox::Sandbox;
use rig::tool::Tool;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn proposal(source_suffix: &str) -> JsProposal {
    JsProposal {
        source: format!("function trim(v) {{ return String(v).trim(); }}{source_suffix}"),
        description: "Trim a value.".to_string(),
        exports: vec![JsExport {
            name: "trim".to_string(),
            signature: "trim(value: unknown): string".to_string(),
        }],
        tests: vec!["trim(' x ') === 'x'".to_string()],
        capability: JsCapability {
            tier: "pure".to_string(),
            grants: vec![],
        },
        tags: vec![" Text ".to_string()],
        predecessor_id: None,
    }
}

#[tokio::test]
async fn propose_skill_host_cancellation_is_bounded_and_next_call_succeeds() {
    let (sender, receiver) = ProposalQueue::bounded(2, Duration::from_secs(1));
    let service = ProposalEffectService::new(ProposalHost::new(sender, AttemptBudget::new(3)));

    let before_dispatch = PermCancellation::new();
    before_dispatch.cancel();
    assert_eq!(
        service
            .execute_cancellable(proposal("-before"), before_dispatch)
            .await,
        Err(EffectServiceError::Cancelled)
    );

    let responder = std::thread::spawn(move || {
        receiver.respond_next(
            Duration::from_millis(50),
            Ok(EnqueueResult {
                proposal_id: "cancelled-proposal".to_string(),
                skill_id: "cancelled-skill".to_string(),
                status: EnqueueStatus::Pending,
                report_id: None,
            }),
        );
        receiver.respond_next(
            Duration::ZERO,
            Ok(EnqueueResult {
                proposal_id: "next-proposal".to_string(),
                skill_id: "next-skill".to_string(),
                status: EnqueueStatus::Pending,
                report_id: None,
            }),
        );
    });
    let cancellation = PermCancellation::new();
    let cancel_later = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel_later.cancel();
    });
    assert_eq!(
        service
            .execute_cancellable(proposal("-during"), cancellation)
            .await,
        Err(EffectServiceError::OutcomeUnknown)
    );
    let next = service
        .execute_cancellable(proposal("-next"), PermCancellation::new())
        .await
        .expect("subsequent proposal succeeds");
    assert_eq!(next.proposal_id, "next-proposal");
    responder.join().unwrap();
}

fn paths() -> (PathBuf, AppPaths) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().join(format!(
        "propose_skill_{}_{}",
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

#[test]
fn propose_skill_validation_builds_full_canonical_identity() {
    let artifact = proposal("")
        .validate_and_canonicalize()
        .expect("valid proposal");
    assert_eq!(artifact.id.len(), 64);
    assert_eq!(artifact.tags, vec!["text"]);
    assert!(artifact.verify_identity().is_ok());
}

#[test]
fn propose_skill_validation_rejects_bounds_and_capability_escalation() {
    let mut oversized = proposal("");
    oversized.source = "x".repeat(32 * 1024 + 1);
    assert!(matches!(
        oversized.validate_and_canonicalize(),
        Err(ProposalError::InvalidField {
            field: "source",
            ..
        })
    ));

    let mut tier_three = proposal("");
    tier_three.capability.tier = "admin".to_string();
    assert!(matches!(
        tier_three.validate_and_canonicalize(),
        Err(ProposalError::InvalidCapability(_))
    ));

    let mut undeclared_tier = proposal("");
    undeclared_tier.capability.grants = vec![JsCapabilityScope::ReadFile {
        workspace_prefixes: vec!["src".to_string()],
    }];
    assert!(matches!(
        undeclared_tier.validate_and_canonicalize(),
        Err(ProposalError::InvalidCapability(_))
    ));
}

#[test]
fn propose_skill_host_budget_is_per_session_and_exact() {
    let first = AttemptBudget::new(2);
    assert!(first.consume().is_ok());
    assert!(first.consume().is_ok());
    assert!(matches!(
        first.consume(),
        Err(ProposalError::BudgetExhausted)
    ));

    let second = AttemptBudget::new(1);
    assert!(second.consume().is_ok(), "budgets must not be global");
}

#[test]
fn propose_skill_backpressure_and_timeout_are_bounded() {
    let (sender, _receiver) = ProposalQueue::bounded(1, Duration::from_millis(5));
    let artifact = proposal("").validate_and_canonicalize().unwrap();
    assert!(matches!(
        sender.enqueue(artifact.clone(), None),
        Err(ProposalError::QueueTimeout)
    ));
    assert!(matches!(
        sender.enqueue(artifact, None),
        Err(ProposalError::QueueFull)
    ));
}

#[test]
fn propose_skill_host_worker_durably_enqueues_idempotently() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("store");
    let worker =
        ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).expect("worker");
    let sender = worker.sender();
    let artifact = proposal("").validate_and_canonicalize().unwrap();

    let first = sender
        .enqueue(artifact.clone(), None)
        .expect("first enqueue");
    let second = sender.enqueue(artifact, None).expect("retry");
    assert_eq!(first.proposal_id, second.proposal_id);
    assert_eq!(first.skill_id, second.skill_id);
    drop(sender);
    drop(worker);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn proposal_host_shutdown_is_bounded_with_live_sender_clones() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("store");
    let worker =
        ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).expect("worker");
    let sender = worker.sender();

    let started = Instant::now();
    drop(worker);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "worker shutdown must not wait for cloned senders"
    );

    let artifact = proposal("").validate_and_canonicalize().unwrap();
    assert!(matches!(
        sender.enqueue(artifact, None),
        Err(ProposalError::QueueClosed)
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn propose_skill_debug_output_redacts_source_and_tests() {
    let proposal = proposal("/* secret-proposal-source */");
    let debug = format!("{proposal:?}");
    assert!(!debug.contains("secret-proposal-source"));
    assert!(!debug.contains("trim(' x ')"));
    assert!(debug.contains("<redacted>"));
}

fn js_payload() -> String {
    serde_json::json!({
        "source": "function trim(v) { return String(v).trim(); }",
        "description": "Trim a value.",
        "exports": [{"name": "trim", "signature": "trim(value: unknown): string"}],
        "tests": ["trim(' x ') === 'x'"],
        "capability": {"tier": "pure", "grants": []},
        "tags": ["text"]
    })
    .to_string()
}

#[tokio::test]
async fn proposal_host_wiring_enqueues_only_in_normal_skills_context() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("store");
    let worker =
        ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).expect("worker");
    let tool = JsTool::new_with_proposals(
        Sandbox::new(false, "bwrap"),
        None,
        None,
        AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        worker,
    );
    assert!(tool.description().contains("propose_skill"));

    let output = tool
        .call(JsArgs {
            code: format!("propose_skill({})", js_payload()),
        })
        .await
        .expect("host call");
    let response: serde_json::Value = serde_json::from_str(&output).expect("structured response");
    assert_eq!(response["status"], "pending");
    assert_eq!(response["id"].as_str().unwrap().len(), 64);
    drop(tool);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn proposal_host_wiring_enforces_session_budget() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("store");
    let worker =
        ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).expect("worker");
    let tool = JsTool::new_with_proposals(
        Sandbox::new(false, "bwrap"),
        None,
        None,
        AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        worker,
    );

    for _ in 0..3 {
        let output = tool
            .call(JsArgs {
                code: format!("propose_skill({})", js_payload()),
            })
            .await
            .expect("allowed attempt");
        assert!(output.contains("\"status\":\"pending\""));
    }
    let exhausted = tool
        .call(JsArgs {
            code: format!("propose_skill({})", js_payload()),
        })
        .await
        .expect("structured JS error");
    assert!(exhausted.contains("proposal attempt budget exhausted"));
    drop(tool);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn proposal_host_validation_budget_precedes_bounded_shape_parsing() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("store");
    let worker =
        ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).expect("worker");
    let tool = JsTool::new_with_proposals(
        Sandbox::new(false, "bwrap"),
        None,
        None,
        AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        worker,
    );

    for _ in 0..3 {
        let output = tool
            .call(JsArgs {
                code: "propose_skill({exports: Array(100000)})".to_string(),
            })
            .await
            .expect("structured validation error");
        assert!(output.contains("invalid exports"));
    }
    let exhausted = tool
        .call(JsArgs {
            code: format!("propose_skill({})", js_payload()),
        })
        .await
        .expect("structured budget error");
    assert!(exhausted.contains("proposal attempt budget exhausted"));
    drop(tool);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn proposal_host_verifier_isolation_omits_symbol_from_all_fake_tiers() {
    use crate::extras::js::skills::verify::verify_skill;

    let artifact = SkillArtifact::new(
        "function checkIsolation() { return typeof propose_skill; }".to_string(),
        "Check verifier host isolation.".to_string(),
        vec!["isolation".to_string()],
        vec![SkillExport {
            name: "checkIsolation".to_string(),
            signature: "checkIsolation(): string".to_string(),
        }],
        vec!["checkIsolation() === 'undefined'".to_string()],
        CapabilityManifest::pure(),
    )
    .expect("artifact");
    assert!(verify_skill(&artifact).is_ok());
}
