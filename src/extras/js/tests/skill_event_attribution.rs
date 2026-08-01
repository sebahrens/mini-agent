use crate::extras::js::skills::telemetry::{
    EventBatch, SkillEvent, SkillEventKind, TelemetryError, TelemetryIngestor, stable_invocation_id,
};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::extras::js::{
    engine::run_instrumented_step_for_test,
    host::AllowConfig,
    tool::PermissionBridgeOwner,
    types::{InstrumentedSkill, JsOutcome, STEP_TIMEOUT, SkillExecutionBundle},
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use crate::sandbox::Sandbox;

fn store() -> (std::path::PathBuf, SkillStore, SkillArtifact) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "phase5-events-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let env = PathEnvironment {
        platform: if cfg!(target_os = "macos") {
            PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            PathPlatform::Windows
        } else {
            PathPlatform::Linux
        },
        home_dir: None,
        config_base: Some(root.clone()),
        data_base: Some(root.clone()),
        local_data_base: Some(root.clone()),
        state_base: Some(root.clone()),
        cache_base: Some(root.clone()),
        workspace_root: None,
        overrides: Default::default(),
    };
    let paths = AppPaths::resolve(&env).unwrap();
    let mut store = SkillStore::open_at(&paths).unwrap();
    let artifact = SkillArtifact::new(
        "function run(x) { return x; }".into(),
        "Event fixture".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "(x) => x".into(),
        }],
        vec!["run(1) === 1".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    store.insert_verified(&artifact).unwrap();
    (root, store, artifact)
}

fn event(
    skill_id: &str,
    invocation_id: &str,
    kind: SkillEventKind,
    outcome: Option<&str>,
) -> SkillEvent {
    SkillEvent {
        invocation_id: Some(invocation_id.into()),
        skill_id: skill_id.into(),
        turn_id: "turn-1".into(),
        tool_call_id: Some("tool-1".into()),
        kind,
        export_name: Some("run".into()),
        outcome: outcome.map(str::to_string),
        latency_us: kind.is_terminal().then_some(17),
        retrieval_score: Some(0.8),
        retrieval_rank: Some(0),
        query_fingerprint: Some("opaque-keyed-fingerprint".into()),
        index_generation: 7,
        evidence_complete: true,
        production: true,
        argument_shape: Some(r#"{"argc":1,"types":["string"]}"#.into()),
        created_at: 10,
    }
}

#[test]
fn stable_ids_reuse_acknowledged_calls_but_separate_ordinals() {
    let first = stable_invocation_id("turn", "tool", "skill", "run", 0);
    assert_eq!(
        first,
        stable_invocation_id("turn", "tool", "skill", "run", 0)
    );
    assert_ne!(
        first,
        stable_invocation_id("turn", "tool", "skill", "run", 1)
    );
    assert_eq!(first.len(), 64);
}

#[test]
fn skill_event_ingestion_is_idempotent_and_counts_only_direct_events_once() {
    let (root, mut store, artifact) = store();
    let invocation = stable_invocation_id("turn-1", "tool-1", &artifact.id, "run", 0);
    let batch = EventBatch::new(vec![
        event(&artifact.id, &invocation, SkillEventKind::Invoked, None),
        event(
            &artifact.id,
            &invocation,
            SkillEventKind::Returned,
            Some("fulfilled"),
        ),
    ])
    .unwrap();
    let mut ingestor = TelemetryIngestor::new(&mut store);
    let first = ingestor.ingest(&batch).unwrap();
    let replay = ingestor.ingest(&batch).unwrap();
    assert_eq!((first.inserted, first.replayed), (2, 0));
    assert_eq!((replay.inserted, replay.replayed), (0, 2));

    let (invoked, succeeded): (i64, i64) = store
        .conn()
        .query_row(
            "SELECT invoked_count, direct_success_count FROM skill_stats
             WHERE skill_id = ?",
            [&artifact.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((invoked, succeeded), (1, 1));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn event_retry_rejects_changed_immutable_metadata_and_deduplicates_selection() {
    let (root, mut store, artifact) = store();
    let invocation = stable_invocation_id("turn-1", "tool-1", &artifact.id, "run", 0);
    let original = event(
        &artifact.id,
        &invocation,
        SkillEventKind::Returned,
        Some("fulfilled"),
    );
    TelemetryIngestor::new(&mut store)
        .ingest(&EventBatch::new(vec![original.clone()]).unwrap())
        .unwrap();
    let mut changed = original;
    changed.latency_us = Some(999);
    assert!(matches!(
        TelemetryIngestor::new(&mut store).ingest(&EventBatch::new(vec![changed]).unwrap()),
        Err(TelemetryError::IdempotencyConflict)
    ));

    let selected = SkillEvent {
        invocation_id: None,
        skill_id: artifact.id.clone(),
        turn_id: "turn-1".into(),
        tool_call_id: Some("tool-1".into()),
        kind: SkillEventKind::Selected,
        export_name: None,
        outcome: None,
        latency_us: None,
        retrieval_score: Some(0.8),
        retrieval_rank: Some(0),
        query_fingerprint: Some("opaque-keyed-fingerprint".into()),
        index_generation: 7,
        evidence_complete: true,
        production: true,
        argument_shape: None,
        created_at: 10,
    };
    let batch = EventBatch::new(vec![selected]).unwrap();
    let first = TelemetryIngestor::new(&mut store).ingest(&batch).unwrap();
    let replay = TelemetryIngestor::new(&mut store).ingest(&batch).unwrap();
    assert_eq!((first.inserted, replay.replayed), (1, 1));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn multiple_terminal_outcomes_fail_before_writing() {
    let (_root, _store, artifact) = store();
    let invocation = stable_invocation_id("turn-1", "tool-1", &artifact.id, "run", 0);
    let result = EventBatch::new(vec![
        event(
            &artifact.id,
            &invocation,
            SkillEventKind::Returned,
            Some("fulfilled"),
        ),
        event(
            &artifact.id,
            &invocation,
            SkillEventKind::Threw,
            Some("exception"),
        ),
    ]);
    assert!(matches!(
        result,
        Err(TelemetryError::MultipleTerminalOutcomes)
    ));
}

#[test]
fn skill_event_privacy_shape_has_no_value_bearing_fields() {
    let (_root, _store, artifact) = store();
    let invocation = stable_invocation_id("turn-1", "tool-1", &artifact.id, "run", 0);
    let secret = "SECRET-CANARY-DO-NOT-PERSIST";
    let safe = event(
        &artifact.id,
        &invocation,
        SkillEventKind::Returned,
        Some("fulfilled"),
    );
    let serialized = serde_json::to_string(&safe).unwrap();
    assert!(!serialized.contains(secret));
    assert!(!serialized.contains("prompt"));
    assert!(!serialized.contains("source"));
    assert!(!serialized.contains("arguments"));

    let mut unsafe_shape = safe;
    unsafe_shape.argument_shape = Some(r#"{"argc":1,"value":"SECRET-CANARY"}"#.into());
    assert!(matches!(
        EventBatch::new(vec![unsafe_shape]),
        Err(TelemetryError::ArgumentShapeTooLarge)
    ));
}

#[tokio::test]
async fn quickjs_wrappers_capture_async_calls_and_unused_selection() {
    let artifact = SkillArtifact::new(
        "function run(value) { return Promise.resolve(value + 1); }".into(),
        "Instrumented async fixture".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "(number) => Promise<number>".into(),
        }],
        vec!["run(1)".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    let bundle = SkillExecutionBundle {
        turn_id: "turn-instrumented".into(),
        tool_call_id: "tool-instrumented".into(),
        index_generation: 3,
        production: true,
        skills: vec![InstrumentedSkill {
            artifact,
            retrieval_score: 0.9,
            retrieval_rank: 0,
            query_fingerprint: Some("v1:opaque".into()),
        }],
    };
    let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
    let bridge = owner.bridge();
    let sandbox = Sandbox::new(false, "bwrap");
    let allow = AllowConfig::unrestricted(&std::env::current_dir().unwrap());
    let response = run_instrumented_step_for_test(
        "run(41)",
        &bundle,
        &sandbox,
        &bridge,
        &crate::extras::js::types::PermCancellation::new(),
        &tokio::runtime::Handle::current(),
        &allow,
    );
    assert_eq!(response.outcome, JsOutcome::Value("42".into()));
    assert_eq!(
        response
            .skill_events
            .iter()
            .filter(|event| event.kind == SkillEventKind::Invoked)
            .count(),
        1
    );
    assert_eq!(
        response
            .skill_events
            .iter()
            .filter(|event| event.kind == SkillEventKind::Returned)
            .count(),
        1
    );
    let shape = response
        .skill_events
        .iter()
        .find(|event| event.kind == SkillEventKind::Invoked)
        .and_then(|event| event.argument_shape.as_deref());
    assert_eq!(shape, Some(r#"{"argc":1,"types":["number"]}"#));

    let unused = run_instrumented_step_for_test(
        "1 + 1",
        &bundle,
        &sandbox,
        &bridge,
        &crate::extras::js::types::PermCancellation::new(),
        &tokio::runtime::Handle::current(),
        &allow,
    );
    assert!(unused.skill_events.iter().any(|event| {
        matches!(
            event.kind,
            SkillEventKind::Selected | SkillEventKind::Injected
        )
    }));
    assert!(
        !unused
            .skill_events
            .iter()
            .any(|event| event.kind == SkillEventKind::Invoked)
    );
    owner.shutdown();
}

#[tokio::test]
async fn undeclared_host_call_is_denied_before_effect_and_directly_attributed() {
    let artifact = SkillArtifact::new(
        "function readSecret() { return read_file('/definitely/not/read'); }".into(),
        "Capability denial fixture".into(),
        vec![],
        vec![SkillExport {
            name: "readSecret".into(),
            signature: "() => string".into(),
        }],
        vec!["typeof readSecret === 'function'".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    let bundle = SkillExecutionBundle {
        turn_id: "turn-denied".into(),
        tool_call_id: "tool-denied".into(),
        index_generation: 4,
        production: true,
        skills: vec![InstrumentedSkill {
            artifact,
            retrieval_score: 0.8,
            retrieval_rank: 0,
            query_fingerprint: None,
        }],
    };
    let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
    let response = run_instrumented_step_for_test(
        "readSecret()",
        &bundle,
        &Sandbox::new(false, "bwrap"),
        &owner.bridge(),
        &crate::extras::js::types::PermCancellation::new(),
        &tokio::runtime::Handle::current(),
        &AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
    );
    assert!(matches!(response.outcome, JsOutcome::Error(_)));
    assert!(
        response
            .skill_events
            .iter()
            .any(|event| event.kind == SkillEventKind::CapabilityDenied),
        "events: {:?}",
        response.skill_events
    );
    owner.shutdown();
}

#[tokio::test]
async fn global_host_escape_is_still_bound_by_the_skill_manifest() {
    let artifact = SkillArtifact::new(
        "async function escapeSkill() {
            await Promise.resolve();
            return globalThis.read_file('/definitely/not/read');
         }"
        .into(),
        "Global capability escape fixture".into(),
        vec![],
        vec![SkillExport {
            name: "escapeSkill".into(),
            signature: "() => string".into(),
        }],
        vec!["typeof escapeSkill === 'function'".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    let bundle = SkillExecutionBundle {
        turn_id: "turn-global-denied".into(),
        tool_call_id: "tool-global-denied".into(),
        index_generation: 4,
        production: true,
        skills: vec![InstrumentedSkill {
            artifact,
            retrieval_score: 0.8,
            retrieval_rank: 0,
            query_fingerprint: None,
        }],
    };
    let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
    let response = run_instrumented_step_for_test(
        "escapeSkill()",
        &bundle,
        &Sandbox::new(false, "bwrap"),
        &owner.bridge(),
        &crate::extras::js::types::PermCancellation::new(),
        &tokio::runtime::Handle::current(),
        &AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
    );
    assert!(matches!(response.outcome, JsOutcome::Error(_)));
    assert!(
        response
            .skill_events
            .iter()
            .any(|event| event.kind == SkillEventKind::CapabilityDenied),
        "events: {:?}",
        response.skill_events
    );
    owner.shutdown();
}
