use rquickjs::context::EvalOptions;
use rquickjs::promise::PromiseState;
use rquickjs::{Context, Error, Function, Persistent, Promise, Runtime, Value};
use std::sync::Arc;

use crate::extras::js::protocol::{EffectResult, GrantId, InvocationId};
use crate::extras::js::realm::{
    BoundExportInvocation, RealmError, call_export_with_capability, load_artifact,
    load_artifact_with_bound_exports, load_artifact_with_capabilities,
};
use crate::extras::js::skills::capability::{
    DispatchedEffect, InvocationAuthorization, InvocationCapabilityRuntime,
};
use crate::extras::js::skills::{
    CapabilityManifest, CapabilityTier, HostCapability, SkillArtifact, SkillExport, test_manifest,
};
use crate::extras::js::types::{MEMORY_LIMIT, STACK_LIMIT};

const MAX_CLONE_BYTES: usize = 1_024;
const MAX_EXCEPTION_MESSAGE_BYTES: usize = 252;
const MAX_EXCEPTION_STACK_BYTES: usize = 32;

#[derive(Debug, Eq, PartialEq)]
enum CloneError {
    Rejected,
    TooLarge,
}

#[test]
fn production_bound_export_is_unreflectable_and_reusable_with_fresh_one_shot_handles() {
    let runtime = Runtime::new().unwrap();
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let model = Context::full(&runtime).unwrap();
    let skill = SkillArtifact::new(
        "function once(_cap, value) { return value + 1; }".into(),
        "One exact production call".into(),
        vec![],
        vec![SkillExport {
            name: "once".into(),
            signature: "(number) => number".into(),
        }],
        vec!["once(1) === 2".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    let capabilities = InvocationCapabilityRuntime::new(|_| {
        panic!("pure bound export must not dispatch an effect")
    });
    let call_capabilities = capabilities.clone();
    let call_skill_id = skill.id.clone();
    let starts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let start_observations = starts.clone();
    let terminals = Arc::new(std::sync::Mutex::new(Vec::new()));
    let terminal_observations = terminals.clone();
    let loaded = load_artifact_with_bound_exports(
        &runtime,
        &model,
        &skill,
        capabilities,
        std::collections::HashMap::from([(
            "once".into(),
            BoundExportInvocation {
                authorize: Arc::new(move |ordinal| {
                    let invocation_id = InvocationId::new(format!("call-{ordinal}")).unwrap();
                    let handle = call_capabilities
                        .prepare(
                            InvocationAuthorization::new(
                                invocation_id,
                                call_skill_id.clone(),
                                "once".into(),
                                CapabilityManifest::pure(),
                                [],
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    Ok((handle, format!("call-{ordinal}")))
                }),
                on_start: Arc::new(move |id, shape| {
                    start_observations.lock().unwrap().push((id, shape));
                    Ok(())
                }),
                on_terminal: Arc::new(move |id, success| {
                    terminal_observations.lock().unwrap().push((id, success));
                    Ok(())
                }),
            },
        )]),
    )
    .unwrap();

    let result = model.with(|ctx| {
        ctx.eval::<String, _>(
            r#"JSON.stringify({
                privateKeys: Reflect.ownKeys(globalThis).filter(key =>
                    typeof key === "string" && key.includes("mini-agent")),
                first: once(41),
                second: once(1)
            })"#,
        )
    });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result.unwrap()).unwrap(),
        serde_json::json!({"privateKeys": [], "first": 42, "second": 2})
    );
    assert_eq!(starts.lock().unwrap().len(), 2);
    assert_eq!(
        terminals.lock().unwrap().as_slice(),
        &[("call-0".into(), true), ("call-1".into(), true)]
    );
    drop(loaded);
    drop(model);
    drop(runtime);
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }

    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn strict_json_encode(context: &Context, candidate_expression: &str) -> Result<String, CloneError> {
    let encoded = context.with(|ctx| {
        let source = format!(
            r#"
            (() => {{
                const candidate = ({candidate_expression});
                const active = new Set();

                function clone(value) {{
                    if (value === null || typeof value === "string" || typeof value === "boolean") {{
                        return value;
                    }}
                    if (typeof value === "number" && Number.isFinite(value)) {{
                        return value;
                    }}
                    if (typeof value !== "object" || active.has(value)) {{
                        throw new TypeError("value is outside the JSON clone contract");
                    }}

                    active.add(value);
                    let copy;
                    if (Array.isArray(value)) {{
                        copy = [];
                        for (let index = 0; index < value.length; index += 1) {{
                            const descriptor = Object.getOwnPropertyDescriptor(value, String(index));
                            if (!descriptor || !descriptor.enumerable || !("value" in descriptor)) {{
                                throw new TypeError("array elements must be enumerable data properties");
                            }}
                            copy.push(clone(descriptor.value));
                        }}
                        const extraKeys = Reflect.ownKeys(value).filter((key) => {{
                            if (key === "length") return false;
                            if (typeof key !== "string") return true;
                            const index = Number(key);
                            return !Number.isInteger(index) || index < 0 || index >= value.length || String(index) !== key;
                        }});
                        if (extraKeys.length !== 0) {{
                            throw new TypeError("array properties are outside the JSON clone contract");
                        }}
                    }} else {{
                        const prototype = Object.getPrototypeOf(value);
                        if (prototype !== Object.prototype && prototype !== null) {{
                            throw new TypeError("host objects are outside the JSON clone contract");
                        }}
                        copy = Object.create(null);
                        for (const key of Reflect.ownKeys(value)) {{
                            if (typeof key !== "string") {{
                                throw new TypeError("symbol keys are outside the JSON clone contract");
                            }}
                            const descriptor = Object.getOwnPropertyDescriptor(value, key);
                            if (!descriptor.enumerable || !("value" in descriptor)) {{
                                throw new TypeError("accessors are outside the JSON clone contract");
                            }}
                            copy[key] = clone(descriptor.value);
                        }}
                    }}
                    active.delete(value);
                    return copy;
                }}

                return JSON.stringify(clone(candidate));
            }})()
            "#
        );

        match ctx.eval::<String, _>(source) {
            Ok(encoded) => Ok(encoded),
            Err(Error::Exception) => {
                let _ = ctx.catch();
                Err(CloneError::Rejected)
            }
            Err(_) => Err(CloneError::Rejected),
        }
    })?;

    if encoded.len() > MAX_CLONE_BYTES {
        return Err(CloneError::TooLarge);
    }
    Ok(encoded)
}

#[test]
fn skill_function_keeps_its_own_global_realm_when_called_from_agent_context() {
    let runtime = Runtime::new().expect("create QuickJS runtime");
    let skill_context = Context::full(&runtime).expect("create skill context");
    let agent_context = Context::full(&runtime).expect("create agent context");

    let skill_function = skill_context.with(|ctx| {
        ctx.globals()
            .set("realm_sentinel", "skill")
            .expect("set skill sentinel");
        let function: Function = ctx
            .eval("() => globalThis.realm_sentinel")
            .expect("create skill function");
        Persistent::save(&ctx, function)
    });

    agent_context.with(|ctx| {
        ctx.globals()
            .set("realm_sentinel", "agent")
            .expect("set agent sentinel");
        let function = skill_function
            .restore(&ctx)
            .expect("restore skill function in the shared runtime");
        let result: String = function.call(()).expect("call cross-context function");
        assert_eq!(result, "skill");
        assert_eq!(
            ctx.eval::<String, _>("globalThis.realm_sentinel")
                .expect("read agent sentinel"),
            "agent"
        );
    });
}

#[test]
fn skill_promise_settles_while_runtime_jobs_are_drained_from_the_agent_step() {
    let runtime = Runtime::new().expect("create QuickJS runtime");
    let skill_context = Context::full(&runtime).expect("create skill context");
    let agent_context = Context::full(&runtime).expect("create agent context");

    let skill_function = skill_context.with(|ctx| {
        ctx.globals()
            .set("realm_sentinel", "skill-continued")
            .expect("set skill sentinel");
        let function: Function = ctx
            .eval("() => Promise.resolve().then(() => globalThis.realm_sentinel)")
            .expect("create async skill function");
        Persistent::save(&ctx, function)
    });

    let promise = agent_context.with(|ctx| {
        ctx.globals()
            .set("realm_sentinel", "agent")
            .expect("set agent sentinel");
        let function = skill_function
            .restore(&ctx)
            .expect("restore async skill function");
        let promise: Promise = function.call(()).expect("call async skill function");
        assert_eq!(promise.state(), PromiseState::Pending);
        Persistent::save(&ctx, promise)
    });

    assert!(runtime.is_job_pending(), "continuation must enqueue a job");
    assert!(
        matches!(runtime.execute_pending_job(), Ok(true)),
        "the agent step must execute at least one runtime job"
    );

    agent_context.with(|ctx| {
        let promise = promise.restore(&ctx).expect("restore settled promise");
        assert_eq!(promise.state(), PromiseState::Resolved);
        let result = promise
            .result::<String>()
            .expect("resolved promise must have a result")
            .expect("convert resolved promise result");
        assert_eq!(result, "skill-continued");
    });
    assert!(
        !runtime.is_job_pending(),
        "the continuation must fully settle"
    );
}

#[test]
fn cross_context_exceptions_preserve_bounded_message_and_stack() {
    let runtime = Runtime::new().expect("create QuickJS runtime");
    let skill_context = Context::full(&runtime).expect("create skill context");
    let agent_context = Context::full(&runtime).expect("create agent context");

    let throwing_function = skill_context.with(|ctx| {
        let mut options = EvalOptions::default();
        options.filename = Some("skill-realm-isolation.js".to_owned());
        let function: Function = ctx
            .eval_with_options(
                "function throw_from_skill() { throw new Error('realm-boom-💥'.repeat(1000)); } throw_from_skill",
                options,
            )
            .expect("create throwing skill function");
        Persistent::save(&ctx, function)
    });

    agent_context.with(|ctx| {
        let function = throwing_function
            .restore(&ctx)
            .expect("restore throwing skill function");
        assert!(matches!(
            function.call::<_, Value>(()),
            Err(Error::Exception)
        ));

        let thrown = function.ctx().catch();
        let exception = thrown
            .as_exception()
            .expect("skill must throw an Error object");
        let message = exception.message().expect("exception message");
        let stack = exception.stack().expect("exception stack");
        assert!(message.starts_with("realm-boom-💥"));
        assert!(stack.contains("throw_from_skill"), "stack was {stack:?}");
        assert!(
            stack.contains("skill-realm-isolation.js"),
            "stack was {stack:?}"
        );

        let bounded_message = truncate_utf8(&message, MAX_EXCEPTION_MESSAGE_BYTES);
        let bounded_stack = truncate_utf8(&stack, MAX_EXCEPTION_STACK_BYTES);
        assert!(message.len() > MAX_EXCEPTION_MESSAGE_BYTES);
        assert!(stack.len() > MAX_EXCEPTION_STACK_BYTES);
        assert!(bounded_message.len() <= MAX_EXCEPTION_MESSAGE_BYTES);
        assert!(bounded_stack.len() <= MAX_EXCEPTION_STACK_BYTES);
        assert!(bounded_message.is_char_boundary(bounded_message.len()));
        assert!(bounded_stack.is_char_boundary(bounded_stack.len()));
        assert!(message.starts_with(&bounded_message));
        assert!(stack.starts_with(&bounded_stack));
    });
}

#[test]
fn skill_context_cannot_resolve_agent_effect_or_proposal_globals() {
    let runtime = Runtime::new().expect("create QuickJS runtime");
    let skill_context = Context::full(&runtime).expect("create skill context");
    let agent_context = Context::full(&runtime).expect("create agent context");

    const GLOBAL_TYPES: &str = r#"[
        typeof read_file,
        typeof write_file,
        typeof fetch,
        typeof spawn,
        typeof propose_skill
    ].join(",")"#;

    agent_context.with(|ctx| {
        ctx.eval::<(), _>(
            "globalThis.read_file = globalThis.write_file = globalThis.fetch =\
             globalThis.spawn = globalThis.propose_skill = () => 'agent authority'",
        )
        .expect("install agent authority globals");
        assert_eq!(
            ctx.eval::<String, _>(GLOBAL_TYPES)
                .expect("inspect agent globals"),
            "function,function,function,function,function"
        );
    });

    skill_context.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>(GLOBAL_TYPES)
                .expect("inspect skill globals"),
            "undefined,undefined,undefined,undefined,undefined"
        );
    });
}

#[test]
fn values_cross_the_boundary_only_through_the_declared_json_clone_contract() {
    let runtime = Runtime::new().expect("create QuickJS runtime");
    let skill_context = Context::full(&runtime).expect("create skill context");
    let agent_context = Context::full(&runtime).expect("create agent context");

    let encoded = strict_json_encode(
        &skill_context,
        r#"({name: "skill", nested: [true, null, 42, {realm: "private"}], ["__proto__"]: {preserved: true}})"#,
    )
    .expect("encode JSON-safe skill value");
    agent_context.with(|ctx| {
        let cloned = ctx.json_parse(encoded).expect("parse clone in agent realm");
        ctx.globals()
            .set("cloned", cloned)
            .expect("publish cloned value");
        assert!(
            ctx.eval::<bool, _>(
                "cloned.name === 'skill' && cloned.nested[3].realm === 'private' &&\
                 Object.prototype.hasOwnProperty.call(cloned, '__proto__') &&\
                 cloned.__proto__.preserved === true &&\
                 Object.getPrototypeOf(cloned) === Object.prototype &&\
                 Object.getPrototypeOf(cloned.nested) === Array.prototype",
            )
            .expect("inspect cloned value")
        );
    });

    for hostile in [
        "() => 1",
        "Symbol('secret')",
        "1n",
        "NaN",
        "Infinity",
        "-Infinity",
        "({nested: undefined})",
        "({nested: () => 1})",
        "({[Symbol('key')]: 'value'})",
        "new Date(0)",
        "Object.defineProperty({}, 'secret', {enumerable: true, get() { throw new Error('getter'); }})",
        "(() => { const value = {}; value.self = value; return value; })()",
        "(() => { const value = []; value.length = 2; value[1] = 'sparse'; return value; })()",
        "(() => { const value = []; value['4294967295'] = 'not-an-index'; return value; })()",
    ] {
        assert_eq!(
            strict_json_encode(&skill_context, hostile),
            Err(CloneError::Rejected),
            "hostile value unexpectedly crossed: {hostile}"
        );
    }

    let array_accessor = r#"(() => {
        globalThis.array_index_getter_calls = 0;
        const value = [];
        Object.defineProperty(value, "0", {
            enumerable: true,
            get() {
                globalThis.array_index_getter_calls += 1;
                return "getter result";
            }
        });
        return value;
    })()"#;
    assert_eq!(
        strict_json_encode(&skill_context, array_accessor),
        Err(CloneError::Rejected)
    );
    skill_context.with(|ctx| {
        assert_eq!(
            ctx.eval::<i32, _>("globalThis.array_index_getter_calls")
                .expect("read array getter canary"),
            0,
            "array index accessors must be rejected without execution"
        );
    });

    let json_string_overhead = strict_json_encode(&skill_context, "''")
        .expect("encode empty string")
        .len();
    let exact_payload_bytes = MAX_CLONE_BYTES
        .checked_sub(json_string_overhead)
        .expect("clone limit must fit an empty JSON string");
    let exact_limit_expression = format!("'x'.repeat({exact_payload_bytes})");
    let exact_limit = strict_json_encode(&skill_context, &exact_limit_expression)
        .expect("an encoding exactly at the clone limit must pass");
    assert_eq!(exact_limit.len(), MAX_CLONE_BYTES);

    let over_limit_expression = format!("'x'.repeat({})", exact_payload_bytes + 1);
    assert_eq!(
        strict_json_encode(&skill_context, &over_limit_expression),
        Err(CloneError::TooLarge)
    );
}

fn artifact(source: &str, exports: &[&str]) -> SkillArtifact {
    SkillArtifact::new(
        source.to_string(),
        "private realm test skill".to_string(),
        vec!["test".to_string()],
        exports
            .iter()
            .map(|name| SkillExport {
                name: (*name).to_string(),
                signature: format!("{name}()"),
            })
            .collect(),
        vec!["true".to_string()],
        CapabilityManifest::pure(),
    )
    .expect("construct identity-v2 artifact")
}

fn bounded_runtime() -> Runtime {
    let runtime = Runtime::new().expect("create runtime");
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    runtime
}

fn grant(byte: u8) -> GrantId {
    GrantId::new(uuid::Uuid::from_bytes([byte; 16])).expect("non-nil grant")
}

fn invocation(value: &str) -> InvocationId {
    InvocationId::new(value).expect("valid invocation id")
}

#[test]
fn abi_v2_hides_an_exact_immutable_capability_argument_and_revokes_retained_methods() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    let manifest = test_manifest(
        CapabilityTier::SideEffecting,
        vec![HostCapability::ReadFile, HostCapability::Spawn],
    )
    .unwrap();
    let skill = SkillArtifact::new(
        "let retained; function inspect(cap, value) { retained = cap.read_file; return {keys: Object.keys(cap), frozen: Object.isFrozen(cap), value, globals: [typeof read_file, typeof spawn, typeof propose_skill]}; } function useRetained(_cap) { try { return retained('secret'); } catch (_) { return 'denied'; } }".into(),
        "ABI-v2 capability fixture".into(),
        vec![],
        vec![
            SkillExport { name: "inspect".into(), signature: "(value)".into() },
            SkillExport { name: "useRetained".into(), signature: "()".into() },
        ],
        vec!["true".into()],
        manifest.clone(),
    )
    .unwrap();
    let dispatched = Arc::new(std::sync::Mutex::new(Vec::<DispatchedEffect>::new()));
    let captured = dispatched.clone();
    let capabilities = InvocationCapabilityRuntime::new(move |effect| {
        captured.lock().unwrap().push(effect);
        Ok(EffectResult::ReadFile {
            content: "unexpected".into(),
        })
    });
    let mut handles = Vec::new();
    for (name, ordinal) in [("inspect", 1), ("useRetained", 2)] {
        handles.push(
            capabilities
                .prepare(
                    InvocationAuthorization::new(
                        invocation(&format!("hidden-{ordinal}")),
                        skill.id.clone(),
                        name.into(),
                        manifest.clone(),
                        [
                            (HostCapability::ReadFile, grant(ordinal)),
                            (HostCapability::Spawn, grant(ordinal + 8)),
                        ],
                    )
                    .unwrap(),
                )
                .unwrap(),
        );
    }

    load_artifact_with_capabilities(&runtime, &model, &skill, capabilities.clone())
        .expect("load capability-bound artifact");
    model.with(|ctx| {
        let inspected: Value = call_export_with_capability(
            &ctx,
            "inspect",
            &capabilities,
            handles[0],
            (41,),
        )
        .unwrap();
        ctx.globals().set("inspected", inspected).unwrap();
        assert_eq!(
            ctx.eval::<String, _>("JSON.stringify(inspected)").unwrap(),
            r#"{"keys":["read_file","spawn"],"frozen":true,"value":41,"globals":["undefined","undefined","undefined"]}"#
        );
        assert_eq!(
            call_export_with_capability::<_, String>(
                &ctx,
                "useRetained",
                &capabilities,
                handles[1],
                (),
            )
            .unwrap(),
            "denied"
        );
    });
    assert!(
        dispatched.lock().unwrap().is_empty(),
        "stale method emitted an effect request"
    );
}

#[test]
fn overlapping_promises_keep_their_explicit_invocation_and_grant_identity() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    let manifest = test_manifest(CapabilityTier::ReadOnly, vec![HostCapability::ReadFile]).unwrap();
    let skill = SkillArtifact::new(
        "async function readLater(cap, path) { await 0; if (path === 'a') await 0; return cap.read_file(path); }".into(),
        "overlap fixture".into(),
        vec![],
        vec![SkillExport { name: "readLater".into(), signature: "(path)".into() }],
        vec!["true".into()],
        manifest.clone(),
    ).unwrap();
    let dispatched = Arc::new(std::sync::Mutex::new(Vec::<DispatchedEffect>::new()));
    let captured = dispatched.clone();
    let capabilities = InvocationCapabilityRuntime::new(move |effect| {
        let path = match &effect.request.operation {
            crate::extras::js::protocol::EffectOperation::ReadFile { path } => path.clone(),
            _ => panic!("unexpected operation"),
        };
        captured.lock().unwrap().push(effect);
        Ok(EffectResult::ReadFile { content: path })
    });
    let mut handles = Vec::new();
    for (name, byte) in [("overlap-a", 3), ("overlap-b", 4)] {
        handles.push(
            capabilities
                .prepare(
                    InvocationAuthorization::new(
                        invocation(name),
                        skill.id.clone(),
                        "readLater".into(),
                        manifest.clone(),
                        [(HostCapability::ReadFile, grant(byte))],
                    )
                    .unwrap(),
                )
                .unwrap(),
        );
    }
    load_artifact_with_capabilities(&runtime, &model, &skill, capabilities.clone())
        .expect("load async artifact");
    let promise = model.with(|ctx| {
        let first: Promise =
            call_export_with_capability(&ctx, "readLater", &capabilities, handles[0], ("a",))
                .unwrap();
        let second: Promise =
            call_export_with_capability(&ctx, "readLater", &capabilities, handles[1], ("b",))
                .unwrap();
        ctx.globals().set("firstPromise", first).unwrap();
        ctx.globals().set("secondPromise", second).unwrap();
        let promise: Promise = ctx
            .eval("Promise.all([firstPromise, secondPromise])")
            .unwrap();
        Persistent::save(&ctx, promise)
    });
    while runtime.is_job_pending() {
        runtime.execute_pending_job().unwrap();
    }
    model.with(|ctx| {
        let promise = promise.restore(&ctx).unwrap();
        assert_eq!(promise.state(), PromiseState::Resolved);
        ctx.globals()
            .set("settled", promise.result::<Value>().unwrap().unwrap())
            .unwrap();
        assert_eq!(
            ctx.eval::<String, _>("JSON.stringify(settled)").unwrap(),
            r#"["a","b"]"#
        );
    });
    let effects = dispatched.lock().unwrap();
    assert_eq!(effects.len(), 2);
    assert_eq!(effects[0].invocation_id.as_str(), "overlap-b");
    assert_eq!(effects[0].request.grant_id, grant(4));
    assert_eq!(effects[0].request.effect_ordinal, 0);
    assert!(matches!(
        &effects[0].request.operation,
        crate::extras::js::protocol::EffectOperation::ReadFile { path } if path == "b"
    ));
    assert_eq!(effects[1].invocation_id.as_str(), "overlap-a");
    assert_eq!(effects[1].request.grant_id, grant(3));
    assert_eq!(effects[1].request.effect_ordinal, 1);
    assert!(matches!(
        &effects[1].request.operation,
        crate::extras::js::protocol::EffectOperation::ReadFile { path } if path == "a"
    ));
}

#[test]
fn unbound_same_export_call_cannot_steal_a_later_invocation_handle() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).unwrap();
    let manifest = test_manifest(CapabilityTier::ReadOnly, vec![HostCapability::ReadFile]).unwrap();
    let skill = SkillArtifact::new(
        "function readExact(cap, path) { return cap.read_file(path); }".into(),
        "one-shot handle theft fixture".into(),
        vec![],
        vec![SkillExport {
            name: "readExact".into(),
            signature: "(path)".into(),
        }],
        vec!["true".into()],
        manifest.clone(),
    )
    .unwrap();
    let effects = Arc::new(std::sync::Mutex::new(Vec::<DispatchedEffect>::new()));
    let captured = effects.clone();
    let capabilities = InvocationCapabilityRuntime::new(move |effect| {
        captured.lock().unwrap().push(effect);
        Ok(EffectResult::ReadFile {
            content: "authorized".into(),
        })
    });
    let handle = capabilities
        .prepare(
            InvocationAuthorization::new(
                invocation("intended-later-call"),
                skill.id.clone(),
                "readExact".into(),
                manifest,
                [(HostCapability::ReadFile, grant(31))],
            )
            .unwrap(),
        )
        .unwrap();
    load_artifact_with_capabilities(&runtime, &model, &skill, capabilities.clone()).unwrap();

    model.with(|ctx| {
        assert!(
            ctx.eval::<bool, _>("try { readExact('attacker'); false } catch (_) { true }",)
                .unwrap()
        );
    });
    assert!(effects.lock().unwrap().is_empty());

    model.with(|ctx| {
        assert_eq!(
            call_export_with_capability::<_, String>(
                &ctx,
                "readExact",
                &capabilities,
                handle,
                ("intended",),
            )
            .unwrap(),
            "authorized"
        );
    });
    let effects = effects.lock().unwrap();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].invocation_id.as_str(), "intended-later-call");
    assert_eq!(effects[0].request.grant_id, grant(31));
}

#[test]
fn async_wrapper_returns_only_a_model_realm_promise_across_private_poisoning() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).unwrap();
    let skill = artifact(
        "async function modelOwned(_cap, value) { await 0; return {value}; } Promise = function PrivatePoison() { throw 0; };",
        &["modelOwned"],
    );
    let capabilities = InvocationCapabilityRuntime::deny_all();
    let handle = capabilities
        .prepare(
            InvocationAuthorization::new(
                invocation("model-owned-promise"),
                skill.id.clone(),
                "modelOwned".into(),
                CapabilityManifest::pure(),
                [],
            )
            .unwrap(),
        )
        .unwrap();
    load_artifact_with_capabilities(&runtime, &model, &skill, capabilities.clone()).unwrap();

    let promise = model.with(|ctx| {
        ctx.eval::<(), _>(
            "globalThis.ExpectedPromise = Promise; globalThis.expectedPromisePrototype = Promise.prototype; globalThis.Promise = function ModelPoison() { throw 0; };",
        )
        .unwrap();
        let promise: Promise = call_export_with_capability(
            &ctx,
            "modelOwned",
            &capabilities,
            handle,
            (17,),
        )
        .unwrap();
        ctx.globals().set("ownedPromise", promise.clone()).unwrap();
        assert!(
            ctx.eval::<bool, _>(
                "ownedPromise instanceof ExpectedPromise && Object.getPrototypeOf(ownedPromise) === expectedPromisePrototype && !(ownedPromise instanceof Promise)",
            )
            .unwrap()
        );
        Persistent::save(&ctx, promise)
    });
    while runtime.is_job_pending() {
        runtime.execute_pending_job().unwrap();
    }
    model.with(|ctx| {
        let promise = promise.restore(&ctx).unwrap();
        assert_eq!(promise.state(), PromiseState::Resolved);
        ctx.globals()
            .set("ownedResult", promise.result::<Value>().unwrap().unwrap())
            .unwrap();
        assert_eq!(
            ctx.eval::<String, _>("JSON.stringify(ownedResult)")
                .unwrap(),
            r#"{"value":17}"#
        );
    });
}

#[test]
fn synchronous_throw_and_promise_rejection_revoke_the_invocation() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).unwrap();
    let skill = artifact(
        "function failSync(_cap) { throw 1; } async function failAsync(_cap) { await Promise.resolve(); throw 2; }",
        &["failSync", "failAsync"],
    );
    let capabilities = InvocationCapabilityRuntime::deny_all();
    let mut handles = Vec::new();
    for export in ["failSync", "failAsync"] {
        handles.push(
            capabilities
                .prepare(
                    InvocationAuthorization::new(
                        invocation(&format!("failure-{export}")),
                        skill.id.clone(),
                        export.into(),
                        CapabilityManifest::pure(),
                        [],
                    )
                    .unwrap(),
                )
                .unwrap(),
        );
    }
    load_artifact_with_capabilities(&runtime, &model, &skill, capabilities.clone()).unwrap();
    model.with(|ctx| {
        assert!(
            call_export_with_capability::<_, Value>(
                &ctx,
                "failSync",
                &capabilities,
                handles[0],
                (),
            )
            .is_err()
        );
        let _ = ctx.catch();
    });
    assert_eq!(capabilities.active_count(), 0);
    let promise = model.with(|ctx| {
        let promise: Promise =
            call_export_with_capability(&ctx, "failAsync", &capabilities, handles[1], ()).unwrap();
        Persistent::save(&ctx, promise)
    });
    while runtime.is_job_pending() {
        runtime.execute_pending_job().unwrap();
    }
    model.with(|ctx| {
        let promise = promise.restore(&ctx).unwrap();
        assert_eq!(promise.state(), PromiseState::Rejected);
        let _ = promise.result::<Value>();
        let _ = ctx.catch();
    });
    assert_eq!(capabilities.active_count(), 0);
}

#[test]
fn method_retained_across_promise_settlement_cannot_dispatch() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).unwrap();
    let manifest = test_manifest(CapabilityTier::ReadOnly, vec![HostCapability::ReadFile]).unwrap();
    let skill = SkillArtifact::new(
        "let retained; async function capture(cap) { retained = cap.read_file; await Promise.resolve(); return true; } function useOld(_cap) { try { return retained('secret'); } catch (_) { return 'denied'; } }".into(),
        "async retained capability fixture".into(), vec![],
        vec![
            SkillExport { name: "capture".into(), signature: "()".into() },
            SkillExport { name: "useOld".into(), signature: "()".into() },
        ], vec!["true".into()], manifest.clone(),
    ).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured = calls.clone();
    let capabilities = InvocationCapabilityRuntime::new(move |_| {
        captured.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(EffectResult::ReadFile {
            content: "unexpected".into(),
        })
    });
    let mut handles = Vec::new();
    for (export, byte) in [("capture", 13), ("useOld", 14)] {
        handles.push(
            capabilities
                .prepare(
                    InvocationAuthorization::new(
                        invocation(&format!("async-{export}")),
                        skill.id.clone(),
                        export.into(),
                        manifest.clone(),
                        [(HostCapability::ReadFile, grant(byte))],
                    )
                    .unwrap(),
                )
                .unwrap(),
        );
    }
    load_artifact_with_capabilities(&runtime, &model, &skill, capabilities.clone()).unwrap();
    model.with(|ctx| {
        call_export_with_capability::<_, Promise>(&ctx, "capture", &capabilities, handles[0], ())
            .unwrap();
    });
    while runtime.is_job_pending() {
        runtime.execute_pending_job().unwrap();
    }
    assert_eq!(capabilities.active_count(), 0);
    model.with(|ctx| {
        assert_eq!(
            call_export_with_capability::<_, String>(
                &ctx,
                "useOld",
                &capabilities,
                handles[1],
                (),
            )
            .unwrap(),
            "denied"
        )
    });
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn production_loader_installs_only_frozen_declared_wrappers() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    let skill = artifact(
        "const secret = 40; function increment(value) { return {value: value + secret}; } function undeclared() { return 0; }",
        &["increment"],
    );

    let loaded = load_artifact(&runtime, &model, &skill).expect("load pure artifact");
    assert_eq!(loaded.artifact_id(), skill.id);
    assert_eq!(loaded.exports(), &["increment"]);
    model.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>(
                "JSON.stringify({result: increment(2), frozen: Object.isFrozen(increment), undeclared: typeof undeclared})",
            )
            .expect("call cloned wrapper"),
            r#"{"result":{"value":42},"frozen":true,"undeclared":"undefined"}"#
        );
        assert!(
            ctx.eval::<bool, _>(
                "(() => { const original = increment; try { increment.extra = 1; increment = () => 0; } catch (_) {} return increment === original && increment.extra === undefined; })()",
            )
            .expect("inspect frozen wrapper")
        );
        assert!(
            ctx.eval::<bool, _>(
                "(() => { const original = increment; const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'increment'); let deleted = true; try { deleted = delete globalThis.increment; } catch (_) { deleted = false; } let redefined = true; try { Object.defineProperty(globalThis, 'increment', {value: () => 0}); } catch (_) { redefined = false; } return descriptor.writable === false && descriptor.configurable === false && deleted === false && redefined === false && increment === original; })()",
            )
            .expect("inspect permanent wrapper binding")
        );
    });
}

#[test]
fn bundled_ajv_is_available_only_inside_private_skill_realms() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    let skill = SkillArtifact::new(
        "function inspectAjv(value) { const valid = Ajv.validate({type: 'string'}, value); return {valid, errors: Ajv.errors, facadeFrozen: Object.isFrozen(Ajv), errorsFrozen: Ajv.errors === null || Object.isFrozen(Ajv.errors), constructorHidden: typeof Ajv.constructor === 'undefined' && typeof Ajv.validate.constructor === 'undefined'}; }".into(),
        "AJV private realm fixture".into(),
        vec![],
        vec![SkillExport {
            name: "inspectAjv".into(),
            signature: "inspectAjv(value)".into(),
        }],
        vec!["inspectAjv('hello').valid === true".into()],
        CapabilityManifest::pure(),
    )
    .expect("create AJV fixture");

    load_artifact(&runtime, &model, &skill).expect("load AJV skill");

    model.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>("typeof Ajv")
                .expect("inspect model global"),
            "undefined"
        );
        assert_eq!(
            ctx.eval::<String, _>("JSON.stringify(inspectAjv('hello'))")
                .expect("validate string"),
            r#"{"valid":true,"errors":null,"facadeFrozen":true,"errorsFrozen":true,"constructorHidden":true}"#
        );
        let invalid = ctx
            .eval::<String, _>("JSON.stringify(inspectAjv(42))")
            .expect("reject number");
        assert!(invalid.contains(r#""valid":false"#));
        assert!(invalid.contains(r#""keyword":"type""#));
        assert!(invalid.contains(r#""facadeFrozen":true"#));
        assert!(invalid.contains(r#""errorsFrozen":true"#));
        assert!(invalid.contains(r#""constructorHidden":true"#));
    });
}

#[test]
fn loader_rejects_collisions_and_missing_exports_without_partial_publication() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    model.with(|ctx| {
        ctx.globals()
            .set("occupied", 7)
            .expect("set occupied global");
    });
    let collision = artifact(
        "throw new Error('collision source must not execute')",
        &["occupied"],
    );
    assert!(matches!(
        load_artifact(&runtime, &model, &collision),
        Err(RealmError::ExportCollision)
    ));

    let missing = artifact(
        "function other() { return 1; }",
        &["missing", "alsoMissing"],
    );
    assert!(matches!(
        load_artifact(&runtime, &model, &missing),
        Err(RealmError::MissingExport)
    ));
    model.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>("[typeof missing, typeof alsoMissing].join(',')")
                .expect("inspect exact publication"),
            "undefined,undefined"
        );
        assert_eq!(ctx.eval::<i32, _>("occupied").unwrap(), 7);
    });
}

#[test]
fn reconstructed_duplicate_exports_are_rejected_before_evaluation() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    let mut duplicate = artifact(
        "throw new Error('duplicate source must not execute')",
        &["same"],
    );
    duplicate.exports.push(duplicate.exports[0].clone());
    duplicate.id = duplicate.compute_identity();

    assert!(matches!(
        load_artifact(&runtime, &model, &duplicate),
        Err(RealmError::DuplicateExport)
    ));
    model.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>("typeof same")
                .expect("inspect atomic publication"),
            "undefined"
        );
    });
}

#[test]
fn loader_rejects_identity_and_abi_mismatch_before_source_evaluation() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    let mut tampered = artifact("function safe() { return 1; }", &["safe"]);
    tampered.source = "throw new Error('must not execute')".to_string();

    assert!(matches!(
        load_artifact(&runtime, &model, &tampered),
        Err(RealmError::Identity)
    ));

    let mut old_abi = artifact("function safe() { return 1; }", &["safe"]);
    old_abi.abi_version = 1;
    old_abi.id = old_abi.compute_identity();
    assert!(matches!(
        load_artifact(&runtime, &model, &old_abi),
        Err(RealmError::Identity)
    ));
    model.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>("typeof safe").expect("inspect model"),
            "undefined"
        );
    });
}

#[test]
fn initialization_has_no_authority_and_rejects_pending_jobs() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    model.with(|ctx| {
        ctx.globals()
            .set("effectCalls", 0)
            .expect("set effect canary");
        ctx.eval::<(), _>(
            "for (const name of ['read_file', 'write_file', 'fetch', 'spawn', 'propose_skill']) { globalThis[name] = () => { globalThis.effectCalls += 1; return 'model'; }; }",
        )
        .expect("install model-only canaries");
    });

    let inspect = artifact(
        "try { read_file('secret'); } catch (_) {} try { write_file('x', 'y'); } catch (_) {} try { fetch('https://example.com'); } catch (_) {} try { spawn('printf', []); } catch (_) {} try { propose_skill({}); } catch (_) {} function inspect() { return [typeof read_file, typeof write_file, typeof fetch, typeof spawn, typeof propose_skill, typeof require, typeof module, typeof exports].join(','); }",
        &["inspect"],
    );
    load_artifact(&runtime, &model, &inspect).expect("load authority-free skill");
    model.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>("inspect()")
                .expect("inspect private globals"),
            "undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined"
        );
        assert_eq!(
            ctx.eval::<i32, _>("effectCalls")
                .expect("inspect effect canary"),
            0,
            "stored initialization must never reach a model effect or proposal host"
        );
    });

    let pending = artifact(
        "Promise.resolve().then(() => 1); function neverInstalled() { return 1; }",
        &["neverInstalled"],
    );
    assert!(matches!(
        load_artifact(&runtime, &model, &pending),
        Err(RealmError::PendingInitializationJobs)
    ));
    model.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>("typeof neverInstalled")
                .expect("inspect rejected export"),
            "undefined"
        );
    });

    let module_source = artifact(
        "import value from 'forbidden'; function moduleLoaded() { return value; }",
        &["moduleLoaded"],
    );
    assert!(matches!(
        load_artifact(&runtime, &model, &module_source),
        Err(RealmError::Initialization)
    ));

    let top_level_return = artifact(
        "return; function returnedFromTopLevel() { return 1; }",
        &["returnedFromTopLevel"],
    );
    assert!(matches!(
        load_artifact(&runtime, &model, &top_level_return),
        Err(RealmError::Initialization)
    ));
}

#[test]
fn export_extraction_uses_data_properties_and_rejects_jobs_queued_by_getters() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    let data_property = artifact(
        "let reads = 0; Object.defineProperty(globalThis, 'readOnce', { get() { reads += 1; return function () { return reads; }; } });",
        &["readOnce"],
    );
    load_artifact(&runtime, &model, &data_property).expect("load getter-backed binding");
    model.with(|ctx| {
        assert_eq!(
            ctx.eval::<i32, _>("readOnce()")
                .expect("call loader-owned data property"),
            1,
            "bridge construction must read the loader namespace data property, not rerun the source getter"
        );
    });

    let queued_getter = artifact(
        "Object.defineProperty(globalThis, 'queuedGetter', { get() { Promise.resolve().then(() => 1); return new Proxy(function () { return 1; }, {}); } });",
        &["queuedGetter"],
    );
    assert!(matches!(
        load_artifact(&runtime, &model, &queued_getter),
        Err(RealmError::PendingInitializationJobs)
    ));
    model.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>("typeof queuedGetter")
                .expect("inspect rejected getter export"),
            "undefined"
        );
    });
}

#[test]
fn private_realms_resist_escape_and_cross_artifact_contamination() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    model.with(|ctx| {
        ctx.globals()
            .set("modelSentinel", "model")
            .expect("set model sentinel");
    });
    let first = artifact(
        "const helper = 40; try { Object.prototype.poisoned = true; } catch (_) {} function first() { let recovered = false; try { recovered = !!({}).constructor.constructor('return this')().modelSentinel; } catch (_) {} return {value: helper + 1, recovered, dynamic: typeof Function, poisoned: ({}).poisoned === true}; }",
        &["first"],
    );
    let second = artifact(
        "const helper = 1; function second() { return {value: helper + 1, seesFirst: typeof first}; }",
        &["second"],
    );

    load_artifact(&runtime, &model, &first).expect("load first private realm");
    load_artifact(&runtime, &model, &second).expect("load second private realm");
    model.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>("JSON.stringify({first: first(), second: second(), model: modelSentinel, modelPoisoned: ({}).poisoned === true})")
                .expect("inspect isolated realms"),
            r#"{"first":{"value":41,"recovered":false,"dynamic":"undefined","poisoned":false},"second":{"value":2,"seesFirst":"undefined"},"model":"model","modelPoisoned":false}"#
        );
    });
}

#[test]
fn wrapper_boundary_rejects_executable_cyclic_accessor_and_async_values() {
    let runtime = bounded_runtime();
    let model = Context::full(&runtime).expect("create model context");
    let skill = artifact(
        "function echo(value) { return value; } function closure() { return () => 1; } function pending() { return Promise.resolve(1); } function oversized() { return 'x'.repeat(70000); } function throwsSecret() { throw new Error('private thrown value'); } function trappedClone() { return new Proxy({safe: 1}, { ownKeys() { throw new Error('private clone trap secret'); } }); } const trappedApply = new Proxy(function () { return 1; }, { apply() { throw new Error('private apply trap secret'); } });",
        &[
            "echo",
            "closure",
            "pending",
            "oversized",
            "throwsSecret",
            "trappedClone",
            "trappedApply",
        ],
    );
    load_artifact(&runtime, &model, &skill).expect("load boundary skill");

    model.with(|ctx| {
        assert!(ctx.eval::<Value, _>("echo(() => 1)").is_err());
        assert!(ctx.eval::<Value, _>("closure()").is_err());
        assert!(ctx.eval::<Value, _>("pending()").is_err());
        assert!(ctx.eval::<Value, _>("oversized()").is_err());
        assert!(ctx.eval::<Value, _>("echo('x'.repeat(70000))").is_err());
        assert!(ctx.eval::<Value, _>("echo(new Date(0))").is_err());
        assert_eq!(
            ctx.eval::<String, _>(
                "(() => { try { throwsSecret(); } catch (value) { return [typeof value, value === 0, String(value)].join(','); } })()",
            )
            .expect("inspect sanitized skill exception"),
            "number,true,0",
            "arbitrary skill exception objects must not cross into the model realm"
        );
        for invocation in ["trappedClone()", "trappedApply()"] {
            let expression = format!(
                "(() => {{ try {{ {invocation}; }} catch (value) {{ return [typeof value, value === 0, String(value)].join(','); }} }})()"
            );
            assert_eq!(
                ctx.eval::<String, _>(expression)
                    .expect("inspect sanitized private Proxy trap"),
                "number,true,0",
                "private parse/apply/clone failures must cross only as the fixed primitive"
            );
        }
        assert!(
            ctx.eval::<Value, _>("(() => { const value = {}; value.self = value; return echo(value); })()")
                .is_err()
        );
        assert!(
            ctx.eval::<Value, _>("echo(Object.defineProperty({}, 'x', {enumerable: true, get() { throw new Error('must not run'); }}))")
                .is_err()
        );
    });
    assert!(
        !runtime.is_job_pending(),
        "a rejected async export must not leave a live continuation"
    );
}

#[test]
fn every_fresh_runtime_gets_new_skill_and_model_realms() {
    for expected in [1, 2, 3] {
        let runtime = bounded_runtime();
        let model = Context::full(&runtime).expect("create model context");
        let source = format!(
            "const privateCounter = 1; function value() {{ return [privateCounter, {expected}]; }}"
        );
        let skill = artifact(&source, &["value"]);
        load_artifact(&runtime, &model, &skill).expect("load into fresh runtime");
        model.with(|ctx| {
            assert_eq!(
                ctx.eval::<String, _>("JSON.stringify(value())")
                    .expect("call fresh wrapper"),
                format!("[1,{expected}]")
            );
            assert_eq!(
                ctx.eval::<String, _>("typeof privateCounter")
                    .expect("inspect model realm"),
                "undefined"
            );
        });
    }
}
