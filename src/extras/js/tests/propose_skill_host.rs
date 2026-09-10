use crate::extras::js::audit::{AuditState, EffectAudit};
use crate::extras::js::broker::{
    GrantPrincipal, HostCapability, InvocationBroker, InvocationGrant,
};
use crate::extras::js::host::AllowConfig;
use crate::extras::js::host::{FileEffectService, ParentHostEffectService, SpawnEffectService};
use crate::extras::js::protocol::{
    AdvisoryAttribution, EffectOperation, EffectRequest, EffectResult, GrantId, InvocationId,
    ProposalStatus, SkillProposalCapability, SkillProposalDraft, SkillProposalExport,
    SkillProposalScope,
};
use crate::extras::js::realm::{
    call_export_with_capability, load_artifact, load_artifact_with_capabilities,
};
use crate::extras::js::skills::capability::{InvocationAuthorization, InvocationCapabilityRuntime};
use crate::extras::js::skills::proposal::{
    AttemptBudget, JsCapability, JsCapabilityScope, JsExport, JsProposal, ProposalEffectService,
    ProposalError, ProposalHost, ProposalQueue, SettledProposal,
};
use crate::extras::js::skills::store::{EnqueueResult, EnqueueStatus, SkillStore};
use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::extras::js::tool::{JsArgs, JsTool, PermissionBridgeOwner};
use crate::extras::js::types::{EffectServiceError, PermCancellation};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use crate::sandbox::Sandbox;
use rig::tool::Tool;
use rquickjs::{Context, Promise, Runtime};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

fn assert_denial_records(audit: &Arc<Mutex<EffectAudit>>, expected: usize) {
    let audit = audit.lock().unwrap();
    assert_eq!(audit.records().len(), expected);
    for record in audit.records() {
        assert_eq!(record.state, AuditState::Completed);
        assert_eq!(record.decision, "denied");
        assert!(record.result_code.is_some());
    }
}

#[tokio::test]
async fn worker_effect_cancellation_bounds_proposal_enqueue_and_next_call_succeeds() {
    let (sender, receiver) = ProposalQueue::bounded(2, Duration::from_secs(5));
    let service = ProposalEffectService::new(ProposalHost::new(sender, AttemptBudget::new(3)));

    let before_dispatch = PermCancellation::new();
    before_dispatch.cancel();
    let prepared = service.authorize_reserved(proposal("-before")).unwrap();
    assert_eq!(
        service
            .execute_prepared_cancellable(prepared, before_dispatch)
            .await,
        Err(EffectServiceError::Cancelled)
    );
    assert!(
        receiver.is_empty(),
        "pre-dispatch cancellation must enqueue nothing"
    );

    let (received, receipt) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let responder = std::thread::spawn(move || {
        receiver.respond_next(|artifact| {
            assert!(artifact.source.ends_with("-during"));
            received.send(()).unwrap();
            released
                .recv_timeout(Duration::from_secs(3))
                .expect("release cancelled response");
            Ok(EnqueueResult {
                proposal_id: "cancelled-proposal".to_string(),
                skill_id: "cancelled-skill".to_string(),
                status: EnqueueStatus::Pending,
                report_id: None,
            })
        });
        receiver.respond_next(|artifact| {
            assert!(artifact.source.ends_with("-next"));
            Ok(EnqueueResult {
                proposal_id: "next-proposal".to_string(),
                skill_id: "next-skill".to_string(),
                status: EnqueueStatus::Pending,
                report_id: None,
            })
        });
    });
    let cancellation = PermCancellation::new();
    let pending_cancellation = cancellation.clone();
    let pending_service = service.clone();
    let prepared = service.authorize_reserved(proposal("-during")).unwrap();
    let pending = tokio::spawn(async move {
        pending_service
            .execute_prepared_cancellable(prepared, pending_cancellation)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), receipt)
        .await
        .unwrap()
        .unwrap();
    cancellation.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap();
    // The queue has the command but cannot reply yet: only unknown outcome is truthful.
    assert_eq!(outcome, Err(EffectServiceError::OutcomeUnknown));
    release.send(()).unwrap();
    let prepared = service.authorize_reserved(proposal("-next")).unwrap();
    let next = service
        .execute_prepared_cancellable(prepared, PermCancellation::new())
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
    let scenario = std::thread::scope(|scope| {
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let shutdown = scope.spawn(move || {
            drop(worker);
            let _ = done_tx.send(());
        });
        let scenario = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            done_rx
                .recv_timeout(Duration::from_secs(15))
                .expect("worker shutdown waited for live sender clones");
            let artifact = proposal("").validate_and_canonicalize().unwrap();
            assert!(matches!(
                sender.enqueue(artifact, None),
                Err(ProposalError::QueueClosed)
            ));
        }));
        // Release clones even if shutdown regresses to waiting for channel closure.
        drop(sender);
        let joined = shutdown.join();
        if scenario.is_ok() {
            joined.expect("shutdown thread panicked");
        }
        scenario
    });
    std::fs::remove_dir_all(root).expect("remove settled proposal fixture");
    if let Err(payload) = scenario {
        std::panic::resume_unwind(payload);
    }
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

fn wire_proposal(source_suffix: &str) -> SkillProposalDraft {
    SkillProposalDraft {
        source: format!("function trim(v) {{ return String(v).trim(); }}{source_suffix}"),
        description: "Trim a value.".to_string(),
        exports: vec![SkillProposalExport {
            name: "trim".to_string(),
            signature: "trim(value: unknown): string".to_string(),
        }],
        tests: vec!["trim(' x ') === 'x'".to_string()],
        capability: SkillProposalCapability {
            tier: "pure".to_string(),
            grants: vec![],
        },
        tags: vec![" Text ".to_string()],
        predecessor_id: None,
    }
}

fn proposal_parent_service(proposal_host: ProposalHost) -> ParentHostEffectService {
    let bridge = PermissionBridgeOwner::new(None, None, Duration::from_secs(1)).bridge();
    ParentHostEffectService::new(
        FileEffectService::new(
            bridge.clone(),
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            Duration::from_secs(1),
        ),
        SpawnEffectService::new(Sandbox::new(false, "bwrap"), bridge, Duration::from_secs(1)),
    )
    .with_proposal(ProposalEffectService::new(proposal_host))
}

#[tokio::test]
async fn model_authored_proposal_uses_one_exact_grant_and_durable_audit_envelope() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("store");
    let worker = ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).unwrap();
    let invocation = InvocationId::new("model-proposal-once").unwrap();
    let grant = InvocationGrant::issue(
        invocation.clone(),
        GrantPrincipal::ModelAuthored {
            tool_call_id: "tool-call-proposal".to_string(),
        },
        BTreeSet::from([HostCapability::ProposeSkill]),
        Instant::now() + Duration::from_secs(10),
    );
    let grant_id = grant.grant_id().clone();
    let audit = Arc::new(Mutex::new(
        crate::extras::js::audit::EffectAudit::open(paths.effect_audit()).unwrap(),
    ));
    let mut broker = InvocationBroker::new(
        invocation,
        vec![grant],
        BTreeSet::from([HostCapability::ProposeSkill]),
        proposal_parent_service(ProposalHost::new(worker.sender(), AttemptBudget::new(1))),
        audit.clone(),
    )
    .unwrap();
    let request = EffectRequest {
        effect_ordinal: 0,
        grant_id,
        advisory: AdvisoryAttribution::default(),
        operation: EffectOperation::ProposeSkill {
            draft: wire_proposal(""),
        },
    };
    let expected_skill_id = JsProposal::try_from(wire_proposal(""))
        .unwrap()
        .validate_and_canonicalize()
        .unwrap()
        .id;

    let accepted = broker
        .dispatch(request.clone(), PermCancellation::new())
        .await
        .expect("model proposal");
    let EffectResult::ProposalAccepted {
        skill_id,
        proposal_id,
        status,
        report_id,
    } = accepted
    else {
        panic!("unexpected proposal response")
    };
    assert_eq!(skill_id, expected_skill_id);
    assert!(!proposal_id.is_empty());
    assert_eq!(status.as_str(), "pending");
    assert_eq!(report_id, None);
    assert!(
        broker
            .dispatch(request, PermCancellation::new())
            .await
            .is_err(),
        "the same invocation/ordinal must not enqueue twice"
    );

    let records = audit.lock().unwrap();
    assert_eq!(records.records().len(), 2);
    assert_eq!(records.records()[0].state, AuditState::Intent);
    assert_eq!(records.records()[0].capability, "propose_skill");
    assert_eq!(records.records()[0].decision, "authorized");
    assert_eq!(records.records()[1].state, AuditState::Completed);
    assert_eq!(
        records.records()[1].result_code.as_deref(),
        Some("succeeded")
    );
    drop(records);
    drop(broker);
    drop(worker);
    let store = SkillStore::open_at(&paths).unwrap();
    assert_eq!(store.count_proposals().unwrap(), 1);
    drop(store);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn proposal_denial_and_cancellation_enqueue_nothing_and_audit_denials() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("store");
    let worker = ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).unwrap();
    let audit = Arc::new(Mutex::new(
        crate::extras::js::audit::EffectAudit::open(paths.effect_audit()).unwrap(),
    ));
    let invocation = InvocationId::new("skill-proposal-denied").unwrap();
    let grant = InvocationGrant::issue(
        invocation.clone(),
        GrantPrincipal::Skill {
            invocation_id: invocation.to_string(),
            artifact_id: "a".repeat(64),
            export: "attack".to_string(),
        },
        BTreeSet::from([HostCapability::ProposeSkill]),
        Instant::now() + Duration::from_secs(10),
    );
    let request = EffectRequest {
        effect_ordinal: 0,
        grant_id: grant.grant_id().clone(),
        advisory: AdvisoryAttribution {
            artifact_id: Some("a".repeat(64)),
            export: Some("attack".to_string()),
        },
        operation: EffectOperation::ProposeSkill {
            draft: wire_proposal("-denied"),
        },
    };
    let mut broker = InvocationBroker::new(
        invocation,
        vec![grant],
        BTreeSet::from([HostCapability::ProposeSkill]),
        proposal_parent_service(ProposalHost::new(worker.sender(), AttemptBudget::new(3))),
        audit.clone(),
    )
    .unwrap();
    assert!(
        broker
            .dispatch(request, PermCancellation::new())
            .await
            .is_err()
    );

    let cancelled_invocation = InvocationId::new("model-proposal-cancelled").unwrap();
    let cancelled_grant = InvocationGrant::issue(
        cancelled_invocation.clone(),
        GrantPrincipal::ModelAuthored {
            tool_call_id: "cancelled-tool-call".to_string(),
        },
        BTreeSet::from([HostCapability::ProposeSkill]),
        Instant::now() + Duration::from_secs(10),
    );
    let mut cancelled_broker = InvocationBroker::new(
        cancelled_invocation,
        vec![cancelled_grant.clone()],
        BTreeSet::from([HostCapability::ProposeSkill]),
        proposal_parent_service(ProposalHost::new(worker.sender(), AttemptBudget::new(3))),
        audit.clone(),
    )
    .unwrap();
    let cancellation = PermCancellation::new();
    cancellation.cancel();
    assert!(
        cancelled_broker
            .dispatch(
                EffectRequest {
                    effect_ordinal: 0,
                    grant_id: cancelled_grant.grant_id().clone(),
                    advisory: AdvisoryAttribution::default(),
                    operation: EffectOperation::ProposeSkill {
                        draft: wire_proposal("-cancelled"),
                    },
                },
                cancellation,
            )
            .await
            .is_err()
    );
    assert_denial_records(&audit, 2);
    drop(broker);
    drop(cancelled_broker);
    drop(worker);
    let store = SkillStore::open_at(&paths).unwrap();
    assert_eq!(store.count_proposals().unwrap(), 0);
    drop(store);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn malformed_parent_proposals_do_not_consume_the_session_budget() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("store");
    let worker = ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).unwrap();
    let audit = Arc::new(Mutex::new(
        crate::extras::js::audit::EffectAudit::open(paths.effect_audit()).unwrap(),
    ));
    let invocation = InvocationId::new("model-proposal-budget").unwrap();
    let grant = InvocationGrant::issue(
        invocation.clone(),
        GrantPrincipal::ModelAuthored {
            tool_call_id: "budget-tool-call".to_string(),
        },
        BTreeSet::from([HostCapability::ProposeSkill]),
        Instant::now() + Duration::from_secs(10),
    );
    let grant_id = grant.grant_id().clone();
    let mut broker = InvocationBroker::new(
        invocation,
        vec![grant],
        BTreeSet::from([HostCapability::ProposeSkill]),
        proposal_parent_service(ProposalHost::new(worker.sender(), AttemptBudget::new(1))),
        audit.clone(),
    )
    .unwrap();
    let mut oversized = wire_proposal("-oversized");
    oversized.source = "x".repeat(crate::extras::js::skills::proposal::MAX_SOURCE_BYTES + 1);
    assert!(
        broker
            .dispatch(
                EffectRequest {
                    effect_ordinal: 0,
                    grant_id: grant_id.clone(),
                    advisory: AdvisoryAttribution::default(),
                    operation: EffectOperation::ProposeSkill { draft: oversized },
                },
                PermCancellation::new(),
            )
            .await
            .is_err()
    );
    // The single session attempt must survive the malformed draft: the broker
    // path validates before reserving, exactly like the direct service path.
    broker
        .dispatch(
            EffectRequest {
                effect_ordinal: 1,
                grant_id,
                advisory: AdvisoryAttribution::default(),
                operation: EffectOperation::ProposeSkill {
                    draft: wire_proposal("-well-formed"),
                },
            },
            PermCancellation::new(),
        )
        .await
        .expect("a well-formed draft still owns the session attempt");

    let records = audit.lock().unwrap();
    assert_eq!(records.records().len(), 3);
    assert_eq!(records.records()[0].state, AuditState::Completed);
    assert_eq!(records.records()[0].decision, "denied");
    assert_eq!(records.records()[1].state, AuditState::Intent);
    assert_eq!(records.records()[1].decision, "authorized");
    assert_eq!(
        records.records()[2].result_code.as_deref(),
        Some("succeeded")
    );
    drop(records);
    drop(broker);
    drop(worker);
    let store = SkillStore::open_at(&paths).unwrap();
    assert_eq!(store.count_proposals().unwrap(), 1);
    drop(store);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn wire_proposal_rejects_33_nested_scope_entries_before_enqueue() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("store");
    let worker = ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).unwrap();
    let audit = Arc::new(Mutex::new(
        crate::extras::js::audit::EffectAudit::open(paths.effect_audit()).unwrap(),
    ));
    let invocation = InvocationId::new("model-proposal-nested-count").unwrap();
    let grant = InvocationGrant::issue(
        invocation.clone(),
        GrantPrincipal::ModelAuthored {
            tool_call_id: "nested-count-tool-call".to_string(),
        },
        BTreeSet::from([HostCapability::ProposeSkill]),
        Instant::now() + Duration::from_secs(10),
    );
    let mut draft = wire_proposal("-nested-count");
    draft.capability = SkillProposalCapability {
        tier: "side_effecting".to_string(),
        grants: vec![SkillProposalScope::Spawn {
            programs: (0..33).map(|index| format!("program{index}")).collect(),
        }],
    };
    let mut broker = InvocationBroker::new(
        invocation,
        vec![grant.clone()],
        BTreeSet::from([HostCapability::ProposeSkill]),
        proposal_parent_service(ProposalHost::new(worker.sender(), AttemptBudget::new(1))),
        audit.clone(),
    )
    .unwrap();

    assert!(
        broker
            .dispatch(
                EffectRequest {
                    effect_ordinal: 0,
                    grant_id: grant.grant_id().clone(),
                    advisory: AdvisoryAttribution::default(),
                    operation: EffectOperation::ProposeSkill { draft },
                },
                PermCancellation::new(),
            )
            .await
            .is_err()
    );
    assert_denial_records(&audit, 1);
    drop(broker);
    drop(worker);
    let store = SkillStore::open_at(&paths).unwrap();
    assert_eq!(store.count_proposals().unwrap(), 0);
    drop(store);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn wire_proposal_rejects_oversized_nested_strings_before_enqueue() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("store");
    let worker = ProposalQueue::start_store_worker(store, 4, Duration::from_secs(1)).unwrap();
    let audit = Arc::new(Mutex::new(
        crate::extras::js::audit::EffectAudit::open(paths.effect_audit()).unwrap(),
    ));
    let invocation = InvocationId::new("model-proposal-nested-strings").unwrap();
    let grant = InvocationGrant::issue(
        invocation.clone(),
        GrantPrincipal::ModelAuthored {
            tool_call_id: "nested-strings-tool-call".to_string(),
        },
        BTreeSet::from([HostCapability::ProposeSkill]),
        Instant::now() + Duration::from_secs(10),
    );
    let grant_id = grant.grant_id().clone();
    let mut broker = InvocationBroker::new(
        invocation,
        vec![grant],
        BTreeSet::from([HostCapability::ProposeSkill]),
        proposal_parent_service(ProposalHost::new(worker.sender(), AttemptBudget::new(3))),
        audit.clone(),
    )
    .unwrap();
    let oversized_origin = format!(
        "https://{}.example",
        "o".repeat(
            crate::extras::js::skills::proposal::MAX_DESCRIPTION_BYTES + 1
                - "https://".len()
                - ".example".len()
        )
    );
    let invalid_scopes = [
        SkillProposalScope::ReadFile {
            workspace_prefixes: vec![
                "p".repeat(crate::extras::js::skills::proposal::MAX_DESCRIPTION_BYTES + 1),
            ],
        },
        SkillProposalScope::Fetch {
            origins: vec![oversized_origin],
            methods: vec!["GET".to_string()],
        },
        SkillProposalScope::Spawn {
            programs: vec!["p".repeat(crate::extras::js::skills::proposal::MAX_TAG_BYTES + 1)],
        },
    ];

    for (effect_ordinal, scope) in invalid_scopes.into_iter().enumerate() {
        let mut draft = wire_proposal(&format!("-nested-string-{effect_ordinal}"));
        draft.capability = SkillProposalCapability {
            tier: "side_effecting".to_string(),
            grants: vec![scope],
        };
        assert!(
            broker
                .dispatch(
                    EffectRequest {
                        effect_ordinal: effect_ordinal as u32,
                        grant_id: grant_id.clone(),
                        advisory: AdvisoryAttribution::default(),
                        operation: EffectOperation::ProposeSkill { draft },
                    },
                    PermCancellation::new(),
                )
                .await
                .is_err()
        );
    }
    assert_denial_records(&audit, 3);
    drop(broker);
    drop(worker);
    let store = SkillStore::open_at(&paths).unwrap();
    assert_eq!(store.count_proposals().unwrap(), 0);
    drop(store);
    let _ = std::fs::remove_dir_all(root);
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
    let description = tool.description();
    assert!(description.contains("propose_skill({source, description, exports"));
    assert!(
        !description.contains("propose_skill(draft): void"),
        "propose_skill returns a JSON string, not void"
    );
    assert!(description.contains("`propose_skill(draft): string`"));
    assert!(description.contains("{id, proposal_id, status, report_id}"));
    assert!(description.contains("canonical immutable revision id"));
    assert!(description.contains("acknowledgement that the candidate was durably queued"));
    for settled in ["awaiting_approval", "rejected", "deferred"] {
        assert!(
            description.contains(settled),
            "the lifecycle sentence must name {settled}"
        );
    }
    assert!(
        description
            .contains("Every test must be a JavaScript expression that returns exactly true")
    );
    assert!(description.contains("repeated and generalizable"));
    assert!(description.contains("Tier is pure, read_only, or side_effecting"));
    assert!(description.contains("{kind: 'fetch', origins, methods}"));
    assert!(description.contains("{kind: 'spawn', programs}"));
    assert!(description.contains("human-gated admission"));
    assert!(!description.contains("spawn(cmd, args)"));

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
async fn model_cannot_execute_a_proposal_in_the_same_step() {
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

    let output = tool
        .call(JsArgs {
            code: format!("propose_skill({}); trim('must-not-run')", js_payload()),
        })
        .await
        .expect("closed execution result");
    assert!(
        output.starts_with("JS ReferenceError at ")
            && output.ends_with("(stage: evaluation; script: model)"),
        "unexpected: {output}"
    );
    drop(tool);

    let store = SkillStore::open_at(&paths).expect("reopen store");
    assert_eq!(store.count_proposals().unwrap(), 1);
    drop(store);
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
    assert!(
        exhausted.starts_with("JS exception at ")
            && exhausted.ends_with("(stage: evaluation; script: model)"),
        "unexpected: {exhausted}"
    );
    drop(tool);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn proposal_host_validation_failures_do_not_consume_the_session_budget() {
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

    for invalid_field in ["capability: {tier: 'admin', grants: []}", "tests: []"] {
        for _ in 0..3 {
            let output = tool
                .call(JsArgs {
                    code: format!("propose_skill({{...{}, {invalid_field}}})", js_payload()),
                })
                .await
                .expect("structured validation error");
            assert!(
                output.starts_with("JS exception at ")
                    && output.ends_with("(stage: evaluation; script: model)"),
                "unexpected: {output}"
            );
        }
    }
    // None of the rejected drafts may have spent an attempt, so the whole
    // session budget is still available to well-formed proposals.
    for _ in 0..3 {
        let output = tool
            .call(JsArgs {
                code: format!("propose_skill({})", js_payload()),
            })
            .await
            .expect("allowed attempt");
        assert!(
            output.contains("\"status\":\"pending\""),
            "unexpected: {output}"
        );
    }
    let exhausted = tool
        .call(JsArgs {
            code: format!("propose_skill({})", js_payload()),
        })
        .await
        .expect("structured budget error");
    assert!(
        exhausted.starts_with("JS exception at ")
            && exhausted.ends_with("(stage: evaluation; script: model)"),
        "unexpected: {exhausted}"
    );
    drop(tool);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn settled_proposal_outcomes_are_observed_once_and_queued_ones_stay_tracked() {
    let (sender, receiver) = ProposalQueue::bounded(2, Duration::from_secs(1));
    let service = ProposalEffectService::new(ProposalHost::new(sender, AttemptBudget::new(3)));
    let responder = std::thread::spawn(move || {
        receiver.respond_next(|_| {
            Ok(EnqueueResult {
                proposal_id: "settling-proposal".to_string(),
                skill_id: "settling-skill".to_string(),
                status: EnqueueStatus::Pending,
                report_id: None,
            })
        });
        // First observation: still queued. Second: settled.
        receiver.respond_next_observation(Ok(None));
        receiver.respond_next_observation(Ok(Some(SettledProposal {
            proposal_id: "settling-proposal".to_string(),
            skill_id: "settling-skill".to_string(),
            status: ProposalStatus::Rejected,
            reason_code: Some("duplicate_skill".to_string()),
            report_id: Some("report-1".to_string()),
        })));
    });

    let prepared = service.authorize_reserved(proposal("")).unwrap();
    service.reserve_attempt().unwrap();
    service.execute_prepared(prepared).expect("enqueue");
    assert!(
        service.settled_outcomes().is_empty(),
        "a queued proposal has no settled outcome yet"
    );
    let settled = service.settled_outcomes();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].proposal_id, "settling-proposal");
    assert_eq!(settled[0].status, ProposalStatus::Rejected);
    assert_eq!(settled[0].reason_code.as_deref(), Some("duplicate_skill"));
    assert!(
        service.settled_outcomes().is_empty(),
        "a settled outcome is reported exactly once"
    );
    responder.join().unwrap();
}

#[test]
fn settled_proposal_outcomes_are_appended_to_the_tool_result() {
    let (sender, receiver) = ProposalQueue::bounded(2, Duration::from_secs(1));
    let service = ProposalEffectService::new(ProposalHost::new(sender, AttemptBudget::new(3)));
    let responder = std::thread::spawn(move || {
        receiver.respond_next(|_| {
            Ok(EnqueueResult {
                proposal_id: "reported-proposal".to_string(),
                skill_id: "reported-skill".to_string(),
                status: EnqueueStatus::Pending,
                report_id: None,
            })
        });
        receiver.respond_next_observation(Ok(Some(SettledProposal {
            proposal_id: "reported-proposal".to_string(),
            skill_id: "reported-skill".to_string(),
            status: ProposalStatus::Deferred,
            reason_code: Some("evaluation_infrastructure_deferred".to_string()),
            report_id: None,
        })));
    });

    let prepared = service.authorize_reserved(proposal("")).unwrap();
    service.reserve_attempt().unwrap();
    service.execute_prepared(prepared).expect("enqueue");
    let rendered = crate::extras::js::tool::with_settled_proposal_outcomes(
        "step result".to_string(),
        &service,
    );
    assert!(rendered.starts_with("step result"));
    assert!(rendered.contains("reported-proposal"));
    assert!(rendered.contains("settled as deferred"));
    assert!(rendered.contains("evaluation_infrastructure_deferred"));
    assert_eq!(
        crate::extras::js::tool::with_settled_proposal_outcomes("next step".to_string(), &service),
        "next step",
        "a reported outcome is not repeated on the next call"
    );
    responder.join().unwrap();
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

#[test]
fn stored_skill_cannot_propose_descendant() {
    let (root, paths) = paths();
    let (proposal_sender, proposal_receiver) = ProposalQueue::bounded(8, Duration::from_millis(5));
    let runtime = Runtime::new().unwrap();
    runtime.set_memory_limit(crate::extras::js::types::MEMORY_LIMIT);
    runtime.set_max_stack_size(crate::extras::js::types::STACK_LIMIT);
    let model = Context::full(&runtime).unwrap();
    crate::extras::js::host::register_proposal_global(
        &model,
        Some(ProposalHost::new(proposal_sender, AttemptBudget::new(32))),
    )
    .unwrap();
    let source = r#"
        const descendant = {
            source: "function child() { return true; }",
            description: "forbidden descendant",
            exports: [{name: "child", signature: "child(): boolean"}],
            tests: ["child() === true"],
            capability: {tier: "pure", grants: []},
            tags: []
        };
        function attemptAll() {
            const attempts = [
                () => propose_skill(descendant),
                () => globalThis["propose_" + "skill"](descendant),
                () => Function("return propose_skill")()(descendant),
                () => ({}).constructor.constructor("return propose_skill")()(descendant),
                () => Object.getPrototypeOf(globalThis).propose_skill(descendant)
            ];
            for (const attempt of attempts) { try { attempt(); } catch (_) {} }
        }
        attemptAll();
        function pureAttack() {
            attemptAll();
            return "denied";
        }
        function effectfulAttack(_cap) {
            attemptAll();
            return Promise.resolve().then(() => { attemptAll(); return "denied"; });
        }
    "#;
    let pure = SkillArtifact::new(
        source.to_string(),
        "Pure stored proposal attack.".to_string(),
        vec![],
        vec![SkillExport {
            name: "pureAttack".to_string(),
            signature: "pureAttack(): Promise<string>".to_string(),
        }],
        vec!["true".to_string()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    load_artifact(&runtime, &model, &pure).expect("load pure attacker");
    model.with(|ctx| {
        assert_eq!(ctx.eval::<String, _>("pureAttack()").unwrap(), "denied");
    });

    let manifest = crate::extras::js::skills::test_manifest(
        crate::extras::js::skills::CapabilityTier::ReadOnly,
        vec![crate::extras::js::skills::HostCapability::ReadFile],
    )
    .unwrap();
    let effectful = SkillArtifact::new(
        source.to_string(),
        "Effectful stored proposal attack.".to_string(),
        vec![],
        vec![SkillExport {
            name: "effectfulAttack".to_string(),
            signature: "effectfulAttack(): Promise<string>".to_string(),
        }],
        vec!["true".to_string()],
        manifest.clone(),
    )
    .unwrap();
    let capabilities = InvocationCapabilityRuntime::new(|_| {
        panic!("stored proposal attempts must not emit any effect request")
    });
    let handle = capabilities
        .prepare(
            InvocationAuthorization::new(
                InvocationId::new("stored-proposal-attack").unwrap(),
                effectful.id.clone(),
                "effectfulAttack".to_string(),
                manifest,
                [(
                    crate::extras::js::skills::HostCapability::ReadFile,
                    GrantId::new(uuid::Uuid::from_bytes([91; 16])).unwrap(),
                )],
            )
            .unwrap(),
        )
        .unwrap();
    load_artifact_with_capabilities(&runtime, &model, &effectful, capabilities.clone())
        .expect("load effectful attacker");
    model.with(|ctx| {
        let promise: Promise =
            call_export_with_capability(&ctx, "effectfulAttack", &capabilities, handle, ())
                .unwrap();
        assert_eq!(promise.finish::<String>().unwrap(), "denied");
    });

    assert!(
        proposal_receiver.is_empty(),
        "stored source/export/promise/constructor lookup must not enqueue"
    );
    let audit = crate::extras::js::audit::EffectAudit::open(paths.effect_audit()).unwrap();
    assert!(audit.records().is_empty());
    drop(audit);
    let _ = std::fs::remove_dir_all(root);
}
