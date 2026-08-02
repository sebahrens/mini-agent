use std::sync::Arc;

use rig::tool::Tool;

use super::make_test_tool;
use crate::extras::js::protocol::{EffectResult, GrantId, InvocationId, MAX_EFFECTS_PER_STEP};
use crate::extras::js::skills::HostCapability;
use crate::extras::js::skills::capability::{
    CapabilityError, InvocationAuthorization, InvocationCapabilityRuntime,
};
use crate::extras::js::skills::turn::{ResolvedSkill, SkillTurnContext, TurnSkillBundle};
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

#[test]
fn cancellation_and_worker_recycle_revoke_before_effect_dispatch() {
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
            content: "should-not-run".into(),
        })
    });
    let skill_id = "a".repeat(64);
    let grant = GrantId::new(uuid::Uuid::from_bytes([7; 16])).unwrap();
    let first = InvocationId::new("cancelled-invocation").unwrap();
    let cancelled_handle = capabilities
        .prepare(
            InvocationAuthorization::new(
                first.clone(),
                skill_id.clone(),
                "run".into(),
                manifest.clone(),
                [(HostCapability::ReadFile, grant.clone())],
            )
            .unwrap(),
        )
        .unwrap();
    let cancelled_token = capabilities
        .begin(cancelled_handle, &skill_id, "run", &manifest)
        .unwrap();
    let lifecycle = WorkerCapabilityLifecycle::new(capabilities.clone());
    lifecycle.cancel(&first);
    assert_eq!(capabilities.active_count(), 0);
    assert!(matches!(
        capabilities.dispatch(cancelled_token, HostCapability::ReadFile, r#"["secret"]"#),
        Err(CapabilityError::Revoked)
    ));

    let recycled_handle = capabilities
        .prepare(
            InvocationAuthorization::new(
                InvocationId::new("recycled-invocation").unwrap(),
                skill_id.clone(),
                "run".into(),
                manifest.clone(),
                [(
                    HostCapability::ReadFile,
                    GrantId::new(uuid::Uuid::from_bytes([9; 16])).unwrap(),
                )],
            )
            .unwrap(),
        )
        .unwrap();
    let recycled_token = capabilities
        .begin(recycled_handle, &skill_id, "run", &manifest)
        .unwrap();
    drop(lifecycle);
    assert_eq!(capabilities.active_count(), 0);
    assert!(matches!(
        capabilities.dispatch(recycled_token, HostCapability::ReadFile, r#"["secret"]"#),
        Err(CapabilityError::Revoked)
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
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
        .begin(first_handle, &skill_id, "first", &manifest)
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
        .begin(first_handle, &skill_id, "run", &manifest)
        .unwrap();
    let second = capabilities
        .begin(second_handle, &skill_id, "run", &manifest)
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
        .begin(after_recycle_handle, &skill_id, "run", &manifest)
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
    let selected = artifact(
        "function increment(_cap, value) { return value + 1; }",
        &["increment"],
        CapabilityManifest::pure(),
    );
    let tool = make_test_tool().with_skill_turn_context(context(vec![resolved(&selected, 0)]));

    let result = tool
        .call(JsArgs {
            code: "increment(41)".to_string(),
        })
        .await
        .unwrap();

    assert_eq!(result, "42");
}

#[tokio::test]
async fn production_runner_emits_parent_bound_invocation_evidence() {
    use crate::extras::js::skills::telemetry::{SkillEventKind, TelemetryDispatcher};

    let selected = artifact(
        "function observed() { return 42; }",
        &["observed"],
        CapabilityManifest::pure(),
    );
    let (tx, rx) = std::sync::mpsc::sync_channel(2);
    let tool = make_test_tool()
        .with_skill_turn_context(context(vec![resolved(&selected, 0)]))
        .with_telemetry(TelemetryDispatcher::from_sender_for_test(tx));

    assert_eq!(
        tool.call(JsArgs {
            code: "observed()".to_string(),
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
        Some(r#"{"argc":0,"types":[]}"#)
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
    let tool = make_test_tool().with_skill_turn_context(context(vec![resolved(&selected, 0)]));

    let result = tool
        .call(JsArgs {
            code: "globalThis.agentCodeRan = true".to_string(),
        })
        .await
        .unwrap();

    assert_eq!(result, "JS error: internal error");
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
    let tool = make_test_tool().with_skill_turn_context(context(vec![resolved(&selected, 0)]));

    let result = tool
        .call(JsArgs {
            code: "1".to_string(),
        })
        .await
        .unwrap();

    assert_eq!(result, "JS error: internal error");
    assert!(!result.contains("ABI-mismatched source must not execute"));
}

#[tokio::test]
async fn duplicate_and_existing_global_exports_fail_closed() {
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
    let duplicate_tool = make_test_tool()
        .with_skill_turn_context(context(vec![resolved(&first, 0), resolved(&second, 1)]));
    let duplicate = duplicate_tool
        .call(JsArgs {
            code: "same()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(duplicate, "JS error: internal error");

    let collision = artifact(
        "function spawn() { return 'shadowed'; }",
        &["spawn"],
        CapabilityManifest::pure(),
    );
    let collision_tool =
        make_test_tool().with_skill_turn_context(context(vec![resolved(&collision, 0)]));
    let collision_result = collision_tool
        .call(JsArgs {
            code: "spawn()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(collision_result, "JS error: internal error");
}

#[tokio::test]
async fn source_and_agent_failures_are_source_free_closed_errors() {
    let broken = artifact(
        "throw new Error('broken selected source')",
        &[],
        CapabilityManifest::pure(),
    );
    let source_tool = make_test_tool().with_skill_turn_context(context(vec![resolved(&broken, 0)]));
    let source_error = source_tool
        .call(JsArgs {
            code: "1".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(source_error, "JS error: internal error");
    assert!(!source_error.contains(&broken.id));

    let agent_tool = make_test_tool();
    let agent_error = agent_tool
        .call(JsArgs {
            code: "throw new Error('broken agent source')".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(agent_error, "JS error: exception");
    assert!(!agent_error.contains("agent.js"));
}

#[tokio::test]
async fn selected_skill_host_calls_require_declared_capabilities() {
    let pure = artifact(
        "function forbidden() { return spawn('printf', ['must-not-run']); }",
        &["forbidden"],
        CapabilityManifest::pure(),
    );
    let pure_tool = make_test_tool().with_skill_turn_context(context(vec![resolved(&pure, 0)]));
    let denied = pure_tool
        .call(JsArgs {
            code: "forbidden()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(denied, "JS error: exception");

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
        make_test_tool().with_skill_turn_context(context(vec![resolved(&allowed, 0)]));
    let result = allowed_tool
        .call(JsArgs {
            code: "permitted()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(result, "allowed");

    let ordinary_agent = make_test_tool()
        .call(JsArgs {
            code: "typeof spawn".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(ordinary_agent, "function");
}

#[tokio::test]
async fn selected_skills_have_private_bindings_and_cannot_export_executable_values() {
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
    let tool = make_test_tool()
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
        make_test_tool().with_skill_turn_context(context(vec![resolved(&escaped, 0)]));
    let denied = escaped_tool
        .call(JsArgs {
            code: "escaped()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(denied, "JS error: exception");
}

#[tokio::test]
async fn selected_skill_source_cannot_replace_protected_host_globals() {
    let selected = artifact(
        "globalThis.spawn = () => 'seized'; function safe() { return 1; }",
        &["safe"],
        CapabilityManifest::pure(),
    );
    let tool = make_test_tool().with_skill_turn_context(context(vec![resolved(&selected, 0)]));
    let result = tool
        .call(JsArgs {
            code: "safe()".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(result, "1");
    assert_eq!(
        make_test_tool()
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
    let tool = make_test_tool().with_skill_turn_context(context(vec![resolved(&selected, 0)]));

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
