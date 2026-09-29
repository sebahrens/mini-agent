use std::sync::Arc;

use rig::tool::Tool;

use super::make_test_tool;
use crate::extras::js::broker::saturate_executable_preparation_slots_for_test;
use crate::extras::js::protocol::{
    EffectResult, GrantId, InvocationId, MAX_EFFECTS_PER_STEP, MAX_SKILL_EXPORTS_PER_ARTIFACT,
};
use crate::extras::js::skills::HostCapability;
use crate::extras::js::skills::capability::{
    CapabilityError, InvocationAuthorization, InvocationCapabilityRuntime,
};
use crate::extras::js::skills::turn::{ResolvedSkill, SkillTurnContext, TurnSkillBundle};
use crate::extras::js::skills::verify::verify_skill;
use crate::extras::js::skills::{
    CapabilityManifest, CapabilityScope, CapabilityTier, SkillArtifact, SkillExport,
};
use crate::extras::js::tool::JsArgs;
use crate::extras::js::worker::WorkerCapabilityLifecycle;

fn artifact(source: &str, exports: &[&str], capability: CapabilityManifest) -> SkillArtifact {
    SkillArtifact::new(
        source.to_string(),
        "runtime binding test skill".to_string(),
        vec!["test".to_string()],
        exports
            .iter()
            .map(|name| SkillExport {
                name: (*name).to_string(),
                signature: format!("{name}()"),
            })
            .collect(),
        vec!["true".to_string()],
        capability,
    )
    .unwrap()
}

fn resolved(artifact: &SkillArtifact, rank: usize) -> ResolvedSkill {
    ResolvedSkill {
        id: artifact.id.clone(),
        identity_version: artifact.identity_version,
        abi_version: artifact.abi_version,
        description: artifact.description.clone(),
        tags: artifact.tags.clone(),
        exports: artifact.exports.clone(),
        tests: artifact.tests.clone(),
        capability: artifact.capability.clone(),
        source: artifact.source.clone(),
        score_bits: 1.0_f32.to_bits(),
        rank,
        route: None,
    }
}

fn context(skills: Vec<ResolvedSkill>) -> Arc<SkillTurnContext> {
    Arc::new(SkillTurnContext::new(TurnSkillBundle {
        turn_id: "binding-turn".to_string(),
        query_fingerprint: "binding-test".to_string(),
        embedding_model_revision: "test-model".to_string(),
        index_generation: 7,
        skills,
    }))
}

async fn call_pure_skill(artifact: &SkillArtifact, code: &str) -> String {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(artifact, 0)]));
    tool.call(JsArgs {
        code: code.to_string(),
    })
    .await
    .expect("skill runtime binding call must succeed without transport retries")
}

fn differential_artifact(source: &str, test: &str) -> SkillArtifact {
    SkillArtifact::new(
        source.to_string(),
        "production/verifier loader differential".to_string(),
        vec!["differential".to_string()],
        vec![SkillExport {
            name: "probe".to_string(),
            signature: "probe()".to_string(),
        }],
        vec![test.to_string()],
        CapabilityManifest::pure(),
    )
    .unwrap()
}

#[tokio::test]
async fn repeated_turn_calls_reuse_compiled_skill_but_not_runtime_state() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let artifact = artifact(
        "let calls = 0; function cached_next() { return ++calls; }",
        &["cached_next"],
        CapabilityManifest::pure(),
    );
    let tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(&artifact, 0)]));

    for _ in 0..2 {
        assert_eq!(
            tool.call(JsArgs {
                code: "cached_next()".to_string(),
            })
            .await
            .unwrap(),
            "1"
        );
    }
}

#[tokio::test]
async fn production_and_verifier_make_identical_loader_decisions_for_differential_artifacts() {
    let cases = [
        (
            "valid",
            "function probe(_cap) { return true; }",
            "probe()",
            "probe()",
        ),
        (
            "raw-vs-wrapper",
            "return; function probe(_cap) { return true; }",
            "probe()",
            "probe()",
        ),
        (
            "top-level-effect",
            "read_file('fixtures/a'); function probe(_cap) { return true; }",
            "probe()",
            "probe()",
        ),
        (
            "ambient-global",
            "const ambient = [typeof read_file, typeof result, typeof scratch_put, typeof scratch_get]; function probe(cap) { return ambient.every(value => value === 'undefined') && typeof cap.read_file === 'undefined'; }",
            "probe()",
            "probe()",
        ),
        (
            "undeclared-export",
            "function other(_cap) { return true; }",
            "true",
            "probe()",
        ),
        (
            "async-export",
            "async function probe(_cap) { return true; }",
            "probe()",
            "probe()",
        ),
        (
            "constructor-escape",
            "function probe(_cap) { let escaped = false; try { escaped = !!({}).constructor.constructor('return this')().modelSentinel; } catch (_) {} return !escaped && typeof Function === 'undefined'; }",
            "probe()",
            "probe()",
        ),
        (
            "stack-behavior",
            "function probe(_cap, depth) { return depth === 0 ? true : probe(_cap, depth - 1); }",
            "probe(100000)",
            "probe(100000)",
        ),
    ];

    for (name, source, test, production_expression) in cases {
        let artifact = differential_artifact(source, test);
        let verifier_accepted = verify_skill(&artifact).is_ok();
        let production = call_pure_skill(&artifact, production_expression).await;
        let production_accepted = production == "true";
        assert_eq!(
            verifier_accepted, production_accepted,
            "loader decision diverged for {name}: production={production:?}"
        );
    }
}

#[test]
fn worker_lifecycle_drop_revokes_active_prepared_and_bound_authority() {
    let manifest = crate::extras::js::skills::test_manifest(
        CapabilityTier::ReadOnly,
        vec![HostCapability::ReadFile],
    )
    .unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured = calls.clone();
    let capabilities = InvocationCapabilityRuntime::new(move |_| {
        captured.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(EffectResult::ReadFile {
            content: "allowed".into(),
        })
    });
    let skill_id = "a".repeat(64);
    let prepare = |name: &str, byte: u8| {
        capabilities
            .prepare(
                InvocationAuthorization::new(
                    InvocationId::new(name).unwrap(),
                    skill_id.clone(),
                    "run".into(),
                    manifest.clone(),
                    [(
                        HostCapability::ReadFile,
                        GrantId::new(uuid::Uuid::from_bytes([byte; 16])).unwrap(),
                    )],
                )
                .unwrap(),
            )
            .unwrap()
    };
    let lifecycle = WorkerCapabilityLifecycle::new(capabilities.clone());
    let active_handle = prepare("active", 7);
    let active_token = capabilities
        .activate_for_test(active_handle, &skill_id, "run", &manifest)
        .unwrap();
    assert_eq!(
        capabilities
            .dispatch(active_token, HostCapability::ReadFile, r#"["before-drop"]"#)
            .unwrap(),
        r#""allowed""#
    );
    let queued_handle = prepare("queued", 8);
    let bound_handle = prepare("bound", 9);
    let stale_binding = capabilities.bind(bound_handle).unwrap();

    drop(lifecycle);
    assert_eq!(capabilities.active_count(), 0);
    assert!(matches!(
        capabilities.dispatch(active_token, HostCapability::ReadFile, r#"["after-drop"]"#),
        Err(CapabilityError::Revoked)
    ));
    assert!(matches!(
        capabilities.claim_bound(&skill_id, "run", &manifest),
        Err(CapabilityError::InvalidInvocation)
    ));
    for handle in [queued_handle, bound_handle] {
        assert!(matches!(
            capabilities.bind(handle),
            Err(CapabilityError::InvalidInvocation)
        ));
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);

    let _fresh_lifecycle = WorkerCapabilityLifecycle::new(capabilities.clone());
    let fresh_handle = prepare("fresh", 10);
    let _fresh_binding = capabilities.bind(fresh_handle).unwrap();
    // An outstanding guard from the retired runtime must not clear a new binding.
    drop(stale_binding);
    let fresh_token = capabilities
        .claim_bound(&skill_id, "run", &manifest)
        .unwrap();
    assert_ne!(fresh_token, active_token);
    assert_eq!(
        capabilities
            .dispatch(fresh_token, HostCapability::ReadFile, r#"["fresh"]"#)
            .unwrap(),
        r#""allowed""#
    );
    assert!(matches!(
        capabilities.dispatch(active_token, HostCapability::ReadFile, r#"["stale"]"#),
        Err(CapabilityError::Revoked)
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[test]
fn prepared_authority_must_contain_exactly_one_grant_per_declared_method() {
    let manifest = crate::extras::js::skills::test_manifest(
        CapabilityTier::SideEffecting,
        vec![HostCapability::ReadFile, HostCapability::Spawn],
    )
    .unwrap();
    assert!(matches!(
        InvocationAuthorization::new(
            InvocationId::new("incomplete-grants").unwrap(),
            "b".repeat(64),
            "run".into(),
            manifest,
            [(
                HostCapability::ReadFile,
                GrantId::new(uuid::Uuid::from_bytes([8; 16])).unwrap()
            )],
        ),
        Err(CapabilityError::InvalidInvocation)
    ));
}

#[test]
fn wrapper_entry_claims_only_the_exact_bound_prepared_handle() {
    let manifest = CapabilityManifest::pure();
    let skill_id = "e".repeat(64);
    let capabilities = InvocationCapabilityRuntime::deny_all();
    let prepare = |invocation: &str, export: &str| {
        capabilities
            .prepare(
                InvocationAuthorization::new(
                    InvocationId::new(invocation).unwrap(),
                    skill_id.clone(),
                    export.into(),
                    manifest.clone(),
                    [],
                )
                .unwrap(),
            )
            .unwrap()
    };
    let second_handle = prepare("prepared-second", "second");
    let first_handle = prepare("prepared-first", "first");

    {
        let _binding = capabilities.bind(second_handle).unwrap();
        assert!(matches!(
            capabilities.claim_bound(&skill_id, "first", &manifest),
            Err(CapabilityError::InvalidInvocation)
        ));
    }
    let _binding = capabilities.bind(second_handle).unwrap();
    let second = capabilities
        .claim_bound(&skill_id, "second", &manifest)
        .unwrap();
    capabilities.finish(second);
    let first = capabilities
        .activate_for_test(first_handle, &skill_id, "first", &manifest)
        .unwrap();
    capabilities.finish(first);
    assert!(capabilities.bind(second_handle).is_err());
}

#[test]
fn all_active_invocations_share_one_effect_ordinal_budget() {
    let manifest = crate::extras::js::skills::test_manifest(
        CapabilityTier::ReadOnly,
        vec![HostCapability::ReadFile],
    )
    .unwrap();
    let skill_id = "f".repeat(64);
    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = effects.clone();
    let capabilities = InvocationCapabilityRuntime::new(move |effect| {
        captured.lock().unwrap().push(effect);
        Ok(EffectResult::ReadFile {
            content: "ok".into(),
        })
    });
    let prepare = |name: &str, byte: u8| {
        capabilities
            .prepare(
                InvocationAuthorization::new(
                    InvocationId::new(name).unwrap(),
                    skill_id.clone(),
                    "run".into(),
                    manifest.clone(),
                    [(
                        HostCapability::ReadFile,
                        GrantId::new(uuid::Uuid::from_bytes([byte; 16])).unwrap(),
                    )],
                )
                .unwrap(),
            )
            .unwrap()
    };
    let first_handle = prepare("aggregate-first", 41);
    let second_handle = prepare("aggregate-second", 42);
    let first = capabilities
        .activate_for_test(first_handle, &skill_id, "run", &manifest)
        .unwrap();
    let second = capabilities
        .activate_for_test(second_handle, &skill_id, "run", &manifest)
        .unwrap();

    for ordinal in 0..MAX_EFFECTS_PER_STEP {
        let token = if ordinal % 2 == 0 { first } else { second };
        capabilities
            .dispatch(token, HostCapability::ReadFile, r#"["allowed"]"#)
            .unwrap();
    }
    assert!(matches!(
        capabilities.dispatch(first, HostCapability::ReadFile, r#"["over-limit"]"#),
        Err(CapabilityError::DispatchDenied)
    ));
    {
        let effects = effects.lock().unwrap();
        assert_eq!(effects.len(), MAX_EFFECTS_PER_STEP as usize);
        assert_eq!(effects.first().unwrap().request.effect_ordinal, 0);
        assert_eq!(
            effects.last().unwrap().request.effect_ordinal,
            MAX_EFFECTS_PER_STEP - 1
        );
        assert!(
            effects
                .windows(2)
                .all(|pair| pair[1].request.effect_ordinal == pair[0].request.effect_ordinal + 1)
        );
    }

    capabilities.recycle();
    let after_recycle_handle = prepare("aggregate-after-recycle", 43);
    let after_recycle = capabilities
        .activate_for_test(after_recycle_handle, &skill_id, "run", &manifest)
        .unwrap();
    capabilities
        .dispatch(
            after_recycle,
            HostCapability::ReadFile,
            r#"["allowed-after-recycle"]"#,
        )
        .unwrap();
    assert_eq!(
        effects
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .request
            .effect_ordinal,
        0
    );
}

#[tokio::test]
async fn selected_skill_exports_are_installed_before_agent_code() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let selected = artifact(
        "function increment(_cap, value) { return value + 1; }",
        &["increment"],
        CapabilityManifest::pure(),
    );
    let tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(&selected, 0)]));

    let result = tool
        .call(JsArgs {
            code: "increment(41)".to_string(),
        })
        .await
        .unwrap();

    assert_eq!(result, "42");
}

#[tokio::test]
async fn selected_skill_export_is_reusable_with_distinct_parent_issued_call_authority() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    use crate::extras::js::skills::telemetry::{SkillEventKind, TelemetryDispatcher};

    let selected = artifact(
        "function read_manifest(cap) { return cap.read_file('Cargo.toml').includes('[package]') ? 'allowed' : 'missing'; }",
        &["read_manifest"],
        CapabilityManifest::new(
            CapabilityTier::ReadOnly,
            vec![CapabilityScope::ReadFile {
                workspace_prefixes: vec!["Cargo.toml".to_string()],
            }],
        )
        .unwrap(),
    );
    let (tx, rx) = std::sync::mpsc::sync_channel(2);
    let tool = make_test_tool(&audit_dirs)
        .with_skill_turn_context(context(vec![resolved(&selected, 0)]))
        .with_telemetry(TelemetryDispatcher::from_sender_for_test(tx));

    assert_eq!(
        tool.call(JsArgs {
            code: "JSON.stringify([read_manifest(), read_manifest()])".to_string(),
        })
        .await
        .unwrap(),
        r#"["allowed","allowed"]"#
    );

    let batch = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("two-call telemetry batch");
    let invoked: Vec<_> = batch
        .events()
        .iter()
        .filter(|event| event.kind == SkillEventKind::Invoked)
        .collect();
    assert_eq!(invoked.len(), 2);
    assert_ne!(invoked[0].invocation_id, invoked[1].invocation_id);
    assert_eq!(
        batch
            .events()
            .iter()
            .filter(|event| event.kind == SkillEventKind::Returned)
            .count(),
        2
    );
}

#[tokio::test]
async fn production_runner_emits_parent_bound_invocation_evidence() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    use crate::extras::js::skills::telemetry::{SkillEventKind, TelemetryDispatcher};

    let selected = artifact(
        "async function observed(_cap, value) { await Promise.resolve(); return value + 1; }",
        &["observed"],
        CapabilityManifest::pure(),
    );
    let (tx, rx) = std::sync::mpsc::sync_channel(2);
    let tool = make_test_tool(&audit_dirs)
        .with_skill_turn_context(context(vec![resolved(&selected, 0)]))
        .with_skill_production(true)
        .with_telemetry(TelemetryDispatcher::from_sender_for_test(tx));

    assert_eq!(
        tool.call(JsArgs {
            code: "await observed(41)".to_string(),
        })
        .await
        .unwrap(),
        "42"
    );

    let batch = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("parent-bound telemetry batch");
    let kinds: Vec<_> = batch.events().iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        vec![
            SkillEventKind::Selected,
            SkillEventKind::Injected,
            SkillEventKind::Invoked,
            SkillEventKind::Returned,
        ]
    );
    assert!(batch.events().iter().all(|event| {
        event.skill_id == selected.id
            && event.turn_id == "binding-turn"
            && event.query_fingerprint.as_deref() == Some("binding-test")
            && event.index_generation == 7
            && event.production
            && event.evidence_complete
    }));
    assert_eq!(
        batch
            .events()
            .iter()
            .find(|event| event.kind == SkillEventKind::Invoked)
            .and_then(|event| event.argument_shape.as_deref()),
        Some(r#"{"argc":1,"types":["number"]}"#)
    );

    assert_eq!(
        tool.call(JsArgs {
            code: "1 + 1".into()
        })
        .await
        .unwrap(),
        "2"
    );
    let unused = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("selected-but-unused telemetry batch");
    assert_eq!(
        unused
            .events()
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![SkillEventKind::Selected, SkillEventKind::Injected],
    );
    assert!(
        unused
            .events()
            .iter()
            .all(|event| event.skill_id == selected.id && event.evidence_complete)
    );
}

#[tokio::test]
async fn identity_mismatch_fails_before_skill_source_runs() {
    let mut selected = artifact(
        "function untouched() { return 1; }",
        &["untouched"],
        CapabilityManifest::pure(),
    );
    selected.source = "throw new Error('source must not execute')".to_string();
    let result = call_pure_skill(&selected, "globalThis.agentCodeRan = true").await;

    assert!(
        result.starts_with("JS error: internal error"),
        "unexpected: {result}"
    );
    assert!(!result.contains("source must not execute"));
}

#[tokio::test]
async fn hidden_capability_abi_mismatch_fails_before_export_source_runs() {
    let mut selected = artifact(
        "throw new Error('ABI-mismatched source must not execute')",
        &["untouched"],
        CapabilityManifest::pure(),
    );
    selected.abi_version = 1;
    selected.id = selected.compute_identity();
    let result = call_pure_skill(&selected, "1").await;

    assert!(
        result.starts_with("JS error: internal error"),
        "unexpected: {result}"
    );
    assert!(!result.contains("ABI-mismatched source must not execute"));
}

#[tokio::test]
async fn duplicate_and_existing_global_exports_fail_closed() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let first = artifact(
        "function same() { return 1; }",
        &["same"],
        CapabilityManifest::pure(),
    );
    let second = artifact(
        "function same() { return 2; }",
        &["same"],
        CapabilityManifest::pure(),
    );
    let duplicate_tool = make_test_tool(&audit_dirs)
        .with_skill_turn_context(context(vec![resolved(&first, 0), resolved(&second, 1)]));
    let duplicate = duplicate_tool
        .call(JsArgs {
            code: "same()".to_string(),
        })
        .await
        .unwrap();
    assert!(
        duplicate.starts_with("JS error: internal error"),
        "unexpected: {duplicate}"
    );

    let collision = artifact(
        "function spawn() { return 'shadowed'; }",
        &["spawn"],
        CapabilityManifest::pure(),
    );
    let collision_tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(&collision, 0)]));
    let collision_result = collision_tool
        .call(JsArgs {
            code: "spawn()".to_string(),
        })
        .await
        .unwrap();
    assert!(
        collision_result.starts_with("JS error: internal error"),
        "unexpected: {collision_result}"
    );
}

#[tokio::test]
async fn source_and_agent_failures_are_source_free_closed_errors() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let broken = artifact(
        "throw new Error('broken selected source')",
        &[],
        CapabilityManifest::pure(),
    );
    let source_tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(&broken, 0)]));
    let source_error = source_tool
        .call(JsArgs {
            code: "1".to_string(),
        })
        .await
        .unwrap();
    assert!(
        source_error.starts_with("JS error: internal error"),
        "unexpected: {source_error}"
    );
    assert!(!source_error.contains(&broken.id));

    let agent_tool = make_test_tool(&audit_dirs);
    let agent_error = agent_tool
        .call(JsArgs {
            code: "throw new Error('broken agent source')".to_string(),
        })
        .await
        .unwrap();
    assert!(
        agent_error.starts_with("JS exception at ")
            && agent_error.ends_with("(stage: evaluation; script: model)"),
        "unexpected: {agent_error}"
    );
    assert!(!agent_error.contains("agent.js"));
}

#[tokio::test]
async fn selected_skill_host_calls_require_declared_capabilities() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let pure = artifact(
        "function forbidden() { return spawn('printf', ['must-not-run']); }",
        &["forbidden"],
        CapabilityManifest::pure(),
    );
    let pure_tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(&pure, 0)]));
    let denied = pure_tool
        .call(JsArgs {
            code: "forbidden()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(denied, "JS exception (stage: evaluation; script: model)");

    let allowed_manifest = CapabilityManifest::new(
        CapabilityTier::ReadOnly,
        vec![CapabilityScope::ReadFile {
            workspace_prefixes: vec!["Cargo.toml".to_string()],
        }],
    )
    .unwrap();
    let allowed = artifact(
        "function permitted(cap) { return cap.read_file('Cargo.toml').includes('[package]') ? 'allowed' : 'missing'; }",
        &["permitted"],
        allowed_manifest,
    );
    let allowed_tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(&allowed, 0)]));
    let result = allowed_tool
        .call(JsArgs {
            code: "permitted()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(result, "allowed");

    let ordinary_agent = make_test_tool(&audit_dirs)
        .call(JsArgs {
            code: "typeof spawn".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(ordinary_agent, "function");
}

#[tokio::test]
async fn production_skill_binding_accepts_distinct_multi_capability_authority_without_effects() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let manifest = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![
            CapabilityScope::ReadFile {
                workspace_prefixes: vec!["Cargo.toml".to_string()],
            },
            CapabilityScope::WriteFile {
                workspace_prefixes: vec!["target".to_string()],
            },
        ],
    )
    .unwrap();
    let selected = artifact(
        "function no_effect(cap) { return typeof cap.read_file === 'function' && typeof cap.write_file === 'function' ? 'bounded' : 'missing'; }",
        &["no_effect"],
        manifest,
    );
    let tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(&selected, 0)]));

    let result = tool
        .call(JsArgs {
            code: "no_effect()".to_string(),
        })
        .await
        .unwrap();

    assert_eq!(result, "bounded");
}

#[tokio::test]
async fn pure_and_read_only_skill_preparation_bypasses_saturated_executable_slots() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let permits = saturate_executable_preparation_slots_for_test().await;
    let pure = artifact(
        "function pure_value() { return 20; }",
        &["pure_value"],
        CapabilityManifest::pure(),
    );
    let read_only = artifact(
        "function read_only_value(cap) { return typeof cap.read_file === 'function' ? 22 : 0; }",
        &["read_only_value"],
        CapabilityManifest::new(
            CapabilityTier::ReadOnly,
            vec![CapabilityScope::ReadFile {
                workspace_prefixes: vec!["Cargo.toml".to_string()],
            }],
        )
        .unwrap(),
    );
    let tool = make_test_tool(&audit_dirs)
        .with_skill_turn_context(context(vec![resolved(&pure, 0), resolved(&read_only, 1)]));

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tool.call(JsArgs {
            code: "pure_value() + read_only_value()".to_string(),
        }),
    )
    .await;
    drop(permits);

    assert_eq!(
        result
            .expect("non-spawn manifests must not wait for executable slots")
            .unwrap(),
        "42"
    );
}

#[tokio::test]
async fn oversized_bundle_is_rejected_before_executable_preparation() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let permits = saturate_executable_preparation_slots_for_test().await;
    let names = (0..MAX_SKILL_EXPORTS_PER_ARTIFACT)
        .map(|index| format!("export_{index}"))
        .collect::<Vec<_>>();
    let exports = names.iter().map(String::as_str).collect::<Vec<_>>();
    let oversized = artifact(
        "function unused() { return 0; }",
        &exports,
        CapabilityManifest::new(
            CapabilityTier::SideEffecting,
            vec![CapabilityScope::Spawn {
                programs: vec!["printf".to_string()],
            }],
        )
        .unwrap(),
    );
    let oversized_bundle = (0..33)
        .map(|rank| resolved(&oversized, rank))
        .collect::<Vec<_>>();
    let tool = make_test_tool(&audit_dirs).with_skill_turn_context(context(oversized_bundle));

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        tool.call(JsArgs {
            code: "1".to_string(),
        }),
    )
    .await;
    drop(permits);

    assert!(
        result
            .expect("bundle bounds must be checked before executable preparation")
            .is_err()
    );
}

#[tokio::test]
async fn selected_skills_have_private_bindings_and_cannot_export_executable_values() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let first = artifact(
        "const helper = 40; function first() { return helper + 1; }",
        &["first"],
        CapabilityManifest::pure(),
    );
    let second = artifact(
        "const helper = 1; function second() { return helper + 1; }",
        &["second"],
        CapabilityManifest::pure(),
    );
    let tool = make_test_tool(&audit_dirs)
        .with_skill_turn_context(context(vec![resolved(&first, 0), resolved(&second, 1)]));
    let result = tool
        .call(JsArgs {
            code: "first() + second()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(result, "43");

    let escaped = artifact(
        "function escaped() { return () => spawn('printf', ['escaped']); }",
        &["escaped"],
        CapabilityManifest::pure(),
    );
    let escaped_tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(&escaped, 0)]));
    let denied = escaped_tool
        .call(JsArgs {
            code: "escaped()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(denied, "JS exception (stage: evaluation; script: model)");
}

#[tokio::test]
async fn selected_skill_source_cannot_replace_protected_host_globals() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let selected = artifact(
        "globalThis.spawn = () => 'seized'; function safe() { return 1; }",
        &["safe"],
        CapabilityManifest::pure(),
    );
    let tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(&selected, 0)]));
    let result = tool
        .call(JsArgs {
            code: "safe()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(result, "1");
    assert_eq!(
        make_test_tool(&audit_dirs)
            .call(JsArgs {
                code: "typeof spawn".to_string(),
            })
            .await
            .unwrap(),
        "function"
    );
}

#[tokio::test]
async fn selected_skill_cannot_recover_the_ambient_realm_or_poison_intrinsics() {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let selected = artifact(
        "const roots = [];\n\
         try { roots.push((0, eval)('this')); } catch (_) {}\n\
         try { roots.push(({}).constructor.constructor('return this')()); } catch (_) {}\n\
         for (const root of roots) {\n\
           if (root && root.spawn) root.Promise.resolve().then(() => root.spawn('printf', ['escaped']));\n\
         }\n\
         try { Object.prototype.skillPolluted = true; } catch (_) {}\n\
         function recoveredAmbientRealm() { return roots.some(root => root && root.spawn); }",
        &["recoveredAmbientRealm"],
        CapabilityManifest::pure(),
    );
    let tool =
        make_test_tool(&audit_dirs).with_skill_turn_context(context(vec![resolved(&selected, 0)]));

    let result = tool
        .call(JsArgs {
            code: "JSON.stringify({ recovered: recoveredAmbientRealm(), polluted: ({}).skillPolluted, dynamicCode: typeof Function })".to_string(),
        })
        .await
        .unwrap();

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result).unwrap(),
        serde_json::json!({"recovered": false, "dynamicCode": "function"})
    );
}

#[test]
fn turn_context_replacement_does_not_mutate_existing_snapshots() {
    let old = artifact(
        "function oldSkill() { return 1; }",
        &["oldSkill"],
        CapabilityManifest::pure(),
    );
    let new = artifact(
        "function newSkill() { return 2; }",
        &["newSkill"],
        CapabilityManifest::pure(),
    );
    let turn_context = context(vec![resolved(&old, 0)]);
    let frozen = turn_context.snapshot();

    turn_context.replace(TurnSkillBundle {
        turn_id: "next-turn-id".to_string(),
        query_fingerprint: "next-turn".to_string(),
        embedding_model_revision: "test-model".to_string(),
        index_generation: 8,
        skills: vec![resolved(&new, 0)],
    });

    assert_eq!(frozen.index_generation, 7);
    assert_eq!(frozen.skills[0].id, old.id);
    assert_eq!(turn_context.snapshot().skills[0].id, new.id);
}

/// Temporary app paths plus an active learned-skill revision and a live index coordinator.
struct ActiveSkillFixture {
    root: std::path::PathBuf,
    paths: crate::paths::AppPaths,
    coordinator: Arc<crate::extras::js::skills::coordinator::IndexCoordinator>,
}

impl ActiveSkillFixture {
    fn new(selected: &SkillArtifact) -> Self {
        use crate::extras::js::skills::coordinator::IndexCoordinator;
        use crate::extras::js::skills::embed::Embedder;
        use crate::extras::js::skills::store::SkillStore;
        use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

        let root = std::env::temp_dir().join(format!("scope-miss-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::resolve(&PathEnvironment {
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
        })
        .unwrap();
        let mut store = SkillStore::open_at(&paths).unwrap();
        store.insert_verified(selected).unwrap();
        drop(store);
        let coordinator =
            Arc::new(IndexCoordinator::open(&paths, Arc::new(Embedder::new().unwrap())).unwrap());
        coordinator.rebuild_and_publish().unwrap();
        let fixture = Self {
            root,
            paths,
            coordinator,
        };
        assert_eq!(fixture.status(&selected.id), "active");
        fixture
    }

    /// Spawn the production telemetry worker outside any Tokio runtime, so dropping the last
    /// handle joins it after it has drained, ingested, and applied automatic quarantine.
    fn telemetry(&self) -> Arc<crate::extras::js::skills::telemetry::TelemetryDispatcher> {
        use crate::extras::js::skills::telemetry::TelemetryDispatcher;
        assert!(tokio::runtime::Handle::try_current().is_err());
        Arc::new(
            TelemetryDispatcher::spawn_session_scoped_with_coordinator(
                &self.paths,
                Arc::clone(&self.coordinator),
            )
            .unwrap(),
        )
    }

    fn status(&self, skill_id: &str) -> String {
        crate::extras::js::skills::store::SkillStore::open_at(&self.paths)
            .unwrap()
            .conn()
            .query_row(
                "SELECT status FROM skill_revisions WHERE id = ?",
                [skill_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn terminal_events(&self, skill_id: &str) -> Vec<(String, Option<String>)> {
        let store = crate::extras::js::skills::store::SkillStore::open_at(&self.paths).unwrap();
        let mut statement = store
            .conn()
            .prepare(
                "SELECT event_kind, outcome FROM skill_events
                  WHERE skill_id = ?
                    AND event_kind IN ('returned','threw','timed_out','oom','capability_denied')
                  ORDER BY event_id",
            )
            .unwrap();
        statement
            .query_map([skill_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
}

impl Drop for ActiveSkillFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// An active-eligible read-scoped skill. Its embedded test uses the pure branch so that store
/// insertion can verify it (both mutation passes are detected) without a host read.
fn read_scoped_docs_skill() -> SkillArtifact {
    SkillArtifact::new(
        "function doc_length(cap, path) { return path === '' ? 0 : cap.read_file(path).length; }"
            .to_string(),
        "read-scoped scope-miss test skill".to_string(),
        vec!["test".to_string()],
        vec![SkillExport {
            name: "doc_length".to_string(),
            signature: "doc_length(path)".to_string(),
        }],
        vec!["doc_length('') === 0".to_string()],
        CapabilityManifest::new(
            CapabilityTier::ReadOnly,
            vec![CapabilityScope::ReadFile {
                workspace_prefixes: vec!["docs".to_string()],
            }],
        )
        .unwrap(),
    )
    .unwrap()
}

/// Model code that passes an out-of-scope path into an active read-scoped skill is caller input,
/// not a skill fault: the effect is denied, the revision stays active, and the call is recorded
/// as non-severe threw-class telemetry (docs/specs/phase-5-evidence-learning.md section 12a.1).
#[test]
fn caller_out_of_scope_target_leaves_active_skill_active_with_non_severe_event() {
    let selected = read_scoped_docs_skill();
    let fixture = ActiveSkillFixture::new(&selected);
    let telemetry = fixture.telemetry();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = runtime.block_on({
        let telemetry = Arc::clone(&telemetry);
        let selected = selected.clone();
        async move {
            let audit_dirs = super::TestTempDir::new("js-test-audits");
            let tool = make_test_tool(&audit_dirs)
                .with_skill_turn_context(context(vec![resolved(&selected, 0)]))
                .with_skill_production(true)
                .with_shared_telemetry(telemetry);
            tool.call(JsArgs {
                code: "let inScope = doc_length('docs/specs/00-index.md') > 0; \
                       let outOfScope; \
                       try { doc_length('README.md'); outOfScope = 'read'; } \
                       catch (_) { outOfScope = 'denied'; } \
                       JSON.stringify([inScope, outOfScope])"
                    .to_string(),
            })
            .await
            .unwrap()
        }
    });
    drop(runtime);
    assert_eq!(output, r#"[true,"denied"]"#);
    // Last handle: joins the worker after ingestion and automatic quarantine evaluation.
    drop(telemetry);

    assert_eq!(
        fixture.terminal_events(&selected.id),
        vec![
            ("returned".to_string(), Some("fulfilled".to_string())),
            ("threw".to_string(), Some("scope_miss".to_string())),
        ]
    );
    assert_eq!(
        fixture.status(&selected.id),
        "active",
        "a caller-derived scope miss must not quarantine the revision"
    );
}

/// A genuine capability-policy fault (an operation outside the skill's granted capability set,
/// recorded by the broker as a policy fault) still quarantines an active revision at once.
#[test]
fn undeclared_capability_policy_fault_still_quarantines_active_skill_immediately() {
    use crate::extras::js::protocol::StepOutcome;
    use crate::extras::js::skills::telemetry::{
        ParentSkillBinding, ParentTelemetryContext, SkillEvent, SkillEventKind, bind_worker_events,
        stable_invocation_id,
    };

    let selected = read_scoped_docs_skill();
    let fixture = ActiveSkillFixture::new(&selected);
    let context = ParentTelemetryContext {
        turn_id: "fault-turn".into(),
        tool_call_id: "fault-turn:js:0".into(),
        query_fingerprint: Some("fault".into()),
        index_generation: 1,
        production: true,
        step_outcome: StepOutcome::Value("ok".into()),
        skills: vec![ParentSkillBinding {
            skill_id: selected.id.clone(),
            exports: ["doc_length".to_string()].into_iter().collect(),
            retrieval_score: 1.0,
            retrieval_rank: 0,
        }],
        capability_denials: Default::default(),
        scope_misses: Default::default(),
    };
    let invocation = stable_invocation_id(
        &context.turn_id,
        &context.tool_call_id,
        &selected.id,
        "doc_length",
        0,
    );
    let claim = |kind: SkillEventKind| SkillEvent {
        invocation_id: (kind != SkillEventKind::Injected).then(|| invocation.clone()),
        skill_id: selected.id.clone(),
        turn_id: context.turn_id.clone(),
        tool_call_id: Some(context.tool_call_id.clone()),
        kind,
        export_name: (kind != SkillEventKind::Injected).then(|| "doc_length".into()),
        outcome: kind.is_terminal().then(|| "exception".into()),
        latency_us: kind.is_terminal().then_some(10),
        retrieval_score: None,
        retrieval_rank: None,
        query_fingerprint: None,
        index_generation: 0,
        evidence_complete: true,
        production: true,
        argument_shape: (kind == SkillEventKind::Invoked)
            .then(|| r#"{"argc":1,"types":["string"]}"#.into()),
        created_at: 1,
    };
    let claims = [
        claim(SkillEventKind::Injected),
        claim(SkillEventKind::Invoked),
        claim(SkillEventKind::Threw),
    ];

    // The same invocation classified as a caller scope miss is not severe ...
    let mut miss_context = context.clone();
    miss_context.scope_misses.insert(invocation.clone());
    let miss = bind_worker_events(&miss_context, &claims).unwrap();
    assert!(
        miss.events()
            .iter()
            .all(|event| event.kind != SkillEventKind::CapabilityDenied)
    );

    // ... but the broker's capability-policy fault record quarantines immediately, without the
    // behavioural threshold.
    let mut fault_context = context;
    fault_context.capability_denials.insert(invocation);
    let fault = bind_worker_events(&fault_context, &claims).unwrap();
    let telemetry = fixture.telemetry();
    telemetry.try_dispatch(fault).unwrap();
    drop(telemetry);

    assert_eq!(
        fixture.terminal_events(&selected.id),
        vec![(
            "capability_denied".to_string(),
            Some("capability_policy".to_string())
        )]
    );
    assert_eq!(fixture.status(&selected.id), "quarantined");
}
