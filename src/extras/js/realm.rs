//! Pure learned-skill realm loading for the Phase 6 worker.
//!
//! A loader invocation creates one private QuickJS context for one immutable identity-v2
//! artifact. Stored source sees no effect, proposal, or module globals. The only model-visible
//! values are frozen wrappers; wrapper arguments and results cross contexts as bounded strict
//! JSON strings. Invocation capability construction is deliberately owned by Phase 6 A17.

use rquickjs::context::EvalOptions;
use rquickjs::function::{Args, IntoArgs, Rest};
use rquickjs::object::Property;
use rquickjs::{
    Context, Ctx, FromJs, Function, Module, Object, Persistent, Runtime, Value, WriteOptions,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use thiserror::Error;

use super::skills::capability::{InvocationCapabilityRuntime, PreparedInvocationHandle};
use super::skills::{
    HostCapability, SKILL_REALM_HARDENING_JS, SkillArtifact, private_skill_source,
};
use super::types::{MEMORY_LIMIT, STACK_LIMIT, STEP_TIMEOUT};
use super::worker::STRICT_CLONE_SOURCE;

type CallAuthorization =
    dyn Fn(u32) -> Result<(PreparedInvocationHandle, String), ()> + Send + Sync + 'static;
type StartObservation = dyn Fn(String, String) -> Result<(), ()> + Send + Sync + 'static;
type TerminalObservation = dyn Fn(String, bool) -> Result<(), ()> + Send + Sync + 'static;

/// One model-visible export's exact parent-prepared call and observation hooks.
#[derive(Clone)]
pub(crate) struct BoundExportInvocation {
    pub(crate) authorize: Arc<CallAuthorization>,
    pub(crate) on_start: Arc<StartObservation>,
    pub(crate) on_terminal: Arc<TerminalObservation>,
}

const BRIDGE_FACTORY_SOURCE: &str = r#"
((parse, apply) => (original, encode) => encodedArguments => {
    try {
        const values = parse(encodedArguments);
        const result = apply(original, undefined, values);
        return encode(result);
    } catch (_) {
        throw 0;
    }
})(JSON.parse, Reflect.apply)
"#;

// AJV's compiler intentionally uses `Function` to turn schemas into validators. It therefore
// lives in a trusted sibling context that evaluates only the vendored bundle and JSON strings;
// its constructor calls route through native QuickJS eval for Windows portability.
// The hardened skill realm receives a frozen facade over a string-only bridge, so neither the
// constructor nor AJV's mutable instance is reachable from stored source or the model realm.
// Skills that do not validate JSON retain the existing resource envelope.
const PRIVATE_SKILL_LIBRARY_MODULE_NAME: &str = "mini-agent:private-skill-library";
const PRIVATE_SKILL_LIBRARY_FACTORY_SOURCE: &str = concat!(
    r#"(function (Function) {
const initialize = function () {
const self = globalThis;
"#,
    include_str!("vendor/ajv.min.js"),
    r#"
const AjvConstructor = globalThis.ajv7.default || globalThis.ajv7;
try {
    const instance = new AjvConstructor({
        allErrors: false, strict: false, validateSchema: false, verbose: false, messages: false,
        code: {optimize: false}, logger: false
    });
    const parse = JSON.parse;
    const stringify = JSON.stringify;
    return function (encodedArguments) {
        let valid;
        try {
            const values = parse(encodedArguments);
            valid = instance.validate(values[0], values[1]);
        } catch (error) {
            try { instance.removeSchema(); } catch (_) {}
            return error === 1
                ? '[false,[{"instancePath":"","schemaPath":"","keyword":"schemaCompilation","params":{}}]]'
                : '[false,[{"instancePath":"","schemaPath":"","keyword":"schemaExecution","params":{}}]]';
        }
        const result = stringify([valid, valid ? null : instance.errors]);
        try {
            instance.removeSchema();
        } catch (_) {
            return '[false,[{"instancePath":"","schemaPath":"","keyword":"schemaCleanup","params":{}}]]';
        }
        return result;
    };
} finally {
    delete globalThis.ajv7;
}
};
return initialize();
})
"#,
);

const PRIVATE_SKILL_LIBRARY_FACADE_SOURCE: &str = r#"
((freeze, parse, stringify, create, defineProperty) => invoke => {
let validationErrors = null;
const freezeErrors = errors => {
    if (errors === null) return null;
    for (const error of errors) {
        if (error.params && typeof error.params === 'object') freeze(error.params);
        freeze(error);
    }
    return freeze(errors);
};
const validate = freeze(function (schema, data) {
    try {
        const result = parse(invoke(stringify([schema, data])));
        validationErrors = freezeErrors(result[1]);
        return result[0] === true;
    } catch (_) {
        validationErrors = freezeErrors([{
            instancePath: '', schemaPath: '', keyword: 'schema', params: {}
        }]);
        return false;
    }
});
const api = create(null);
defineProperty(api, 'validate', {
    value: validate, enumerable: true, writable: false, configurable: false
});
defineProperty(api, 'errors', {
    get: freeze(() => validationErrors), enumerable: true, configurable: false
});
freeze(api);
defineProperty(globalThis, 'Ajv', {
    value: api, enumerable: false, writable: false, configurable: false
});
})(Object.freeze, JSON.parse, JSON.stringify, Object.create, Object.defineProperty)
"#;

static PRIVATE_SKILL_LIBRARY_BYTECODE: OnceLock<Option<Vec<u8>>> = OnceLock::new();

fn compile_private_skill_library_bytecode() -> rquickjs::Result<Vec<u8>> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + STEP_TIMEOUT;
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    let context = Context::full(&runtime)?;
    context.with(|ctx| {
        let source = format!("export const install = {PRIVATE_SKILL_LIBRARY_FACTORY_SOURCE};");
        Module::declare(ctx, PRIVATE_SKILL_LIBRARY_MODULE_NAME, source)?
            .write(WriteOptions::default())
    })
}

fn private_skill_library_bytecode() -> Option<&'static [u8]> {
    PRIVATE_SKILL_LIBRARY_BYTECODE
        .get_or_init(|| compile_private_skill_library_bytecode().ok())
        .as_deref()
}

fn artifact_uses_ajv(artifact: &SkillArtifact) -> bool {
    artifact.source.contains("Ajv") || artifact.tests.iter().any(|test| test.contains("Ajv"))
}

fn compile_private_library_function<'js>(
    ctx: Ctx<'js>,
    encoded_parts: String,
) -> rquickjs::Result<Function<'js>> {
    let parts: Vec<String> =
        serde_json::from_str(&encoded_parts).map_err(|_| rquickjs::Error::Unknown)?;
    let (body, parameters) = parts.split_last().ok_or(rquickjs::Error::Unknown)?;
    // AJV supplies the same parameter/body strings it would pass to the standard Function
    // constructor. This trusted shim changes only the QuickJS API used to compile them.
    let source = format!("(function({}) {{\n{}\n}})", parameters.join(","), body);
    ctx.eval(source)
}

#[allow(unsafe_code)]
fn build_private_skill_library_bridge(
    runtime: &Runtime,
    bytecode: &[u8],
) -> Result<Persistent<Function<'static>>, RealmError> {
    let context = Context::full(runtime).map_err(|_| RealmError::PrivateLibraryBytecodeLoad)?;
    context.with(|ctx| {
        // SAFETY: the bytes are compiled once in this process from the checked-in trusted bundle,
        // with the same linked QuickJS ABI, and are never accepted from disk, IPC, or model output.
        let module = unsafe { Module::load(ctx.clone(), bytecode) }
            .map_err(|_| RealmError::PrivateLibraryBytecodeLoad)?;
        let (module, evaluation) = module
            .eval()
            .map_err(|_| RealmError::PrivateLibraryModuleEvaluation)?;
        evaluation
            .finish::<()>()
            .map_err(|_| RealmError::PrivateLibraryModuleEvaluation)?;
        let install = module
            .get::<_, Function>("install")
            .map_err(|_| RealmError::PrivateLibraryExportLookup)?;
        let compiler = Function::new(ctx.clone(), compile_private_library_function)
            .map_err(|_| RealmError::PrivateLibraryExportLookup)?;
        let constructor_factory = ctx
            .eval::<Function, _>(
                "((stringify) => compile => function (...parts) { try { return compile(stringify(parts)); } catch (_) { throw 1; } })(JSON.stringify)",
            )
            .map_err(|_| RealmError::PrivateLibraryExportLookup)?;
        let function_constructor = constructor_factory
            .call::<_, Function>((compiler,))
            .map_err(|_| RealmError::PrivateLibraryExportLookup)?;
        let bridge = install
            .call::<_, Function>((function_constructor,))
            .map_err(|_| RealmError::PrivateLibraryFactoryExecution)?;
        Ok(Persistent::save(&ctx, bridge))
    })
}

fn install_private_skill_library_facade(
    ctx: &Ctx<'_>,
    bridge: Persistent<Function<'static>>,
) -> Result<(), RealmError> {
    let factory = ctx
        .eval::<Function, _>(PRIVATE_SKILL_LIBRARY_FACADE_SOURCE)
        .map_err(|_| RealmError::PrivateLibraryFactoryExecution)?;
    let bridge = bridge
        .restore(ctx)
        .map_err(|_| RealmError::PrivateLibraryFactoryExecution)?;
    factory
        .call::<_, ()>((bridge,))
        .map_err(|_| RealmError::PrivateLibraryFactoryExecution)
}

const PURE_MODEL_WRAPPER_FACTORY_SOURCE: &str = r#"
((freeze, parse) => (invoke, encode) => freeze(function (...values) {
    return parse(invoke(encode(values)));
}))(Object.freeze, JSON.parse)
"#;

const MODEL_WRAPPER_FACTORY_SOURCE: &str = r#"
((freeze, parse, PromiseCtor) =>
 (invoke, encode, claim, revoke, prepareSettlement, abandonSettlement) => freeze(function (...values) {
    const token = claim();
    let settlementId;
    try {
        let resolveEncoded;
        let rejectEncoded;
        const publicPromise = new PromiseCtor((resolve, reject) => {
            resolveEncoded = encoded => {
                try { resolve(parse(encoded)); } catch (_) { reject(0); }
            };
            rejectEncoded = () => reject(0);
        });
        settlementId = prepareSettlement(resolveEncoded, rejectEncoded);
        const encoded = invoke(token, encode(values), settlementId);
        if (encoded === undefined) return publicPromise;
        abandonSettlement(settlementId);
        return parse(encoded);
    } catch (_) {
        if (settlementId !== undefined) abandonSettlement(settlementId);
        revoke(token);
        throw 0;
    }
}))(Object.freeze, JSON.parse, Promise)
"#;

const TERMINAL_WRAPPER_SOURCE: &str = r#"
((apply, promiseResolve, promiseThen, PromiseCtor) =>
 (result, invocationId, onTerminal) => {
    if (result && typeof result.then === "function") {
        const promise = apply(promiseResolve, PromiseCtor, [result]);
        return apply(promiseThen, promise, [
            value => { onTerminal(invocationId, true); return value; },
            error => { onTerminal(invocationId, false); throw error; }
        ]);
    }
    onTerminal(invocationId, true);
    return result;
})(Reflect.apply, Promise.resolve, Promise.prototype.then, Promise)
"#;

const CAPABILITY_BRIDGE_FACTORY_SOURCE: &str = r#"
((parse, apply, freeze, create, defineProperty, promiseResolve, promiseThen, PromiseCtor) =>
 (original, encode, dispatch, finish) =>
 (settleSuccess, settleFailure, methods) =>
 (token, encodedArguments, settlementId) => {
    try {
        const capability = create(null);
        for (const method of methods) {
            const invokeEffect = freeze(function (...effectArguments) {
                try {
                    return parse(dispatch(token, method, encode(effectArguments)));
                } catch (_) {
                    throw 0;
                }
            });
            defineProperty(capability, method, {
                value: invokeEffect, enumerable: true, writable: false, configurable: false
            });
        }
        freeze(capability);
        const values = parse(encodedArguments);
        const result = apply(original, undefined, [capability, ...values]);
        if (result && typeof result.then === "function") {
            const privatePromise = apply(promiseResolve, PromiseCtor, [result]);
            apply(promiseThen, privatePromise, [
                value => {
                    try { settleSuccess(settlementId, encode(value)); }
                    catch (_) { try { settleFailure(settlementId); } catch (_) {} }
                    finally { finish(token); }
                },
                _error => {
                    try { settleFailure(settlementId); } catch (_) {}
                    finally { finish(token); }
                }
            ]);
            return undefined;
        }
        try { return encode(result); } finally { finish(token); }
    } catch (_) {
        finish(token);
        throw 0;
    }
})(JSON.parse, Reflect.apply, Object.freeze, Object.create, Object.defineProperty,
   Promise.resolve, Promise.prototype.then, Promise)
"#;

#[derive(Default)]
struct ModelSettlementRegistry {
    state: Mutex<ModelSettlementState>,
}

#[derive(Default)]
struct ModelSettlementState {
    next_id: u64,
    pending: HashMap<u64, ModelSettlement>,
}

struct ModelSettlement {
    resolve: Persistent<Function<'static>>,
    reject: Persistent<Function<'static>>,
}

impl ModelSettlementRegistry {
    fn prepare(
        &self,
        resolve: Persistent<Function<'static>>,
        reject: Persistent<Function<'static>>,
    ) -> rquickjs::Result<u64> {
        let mut state = self.state.lock().map_err(|_| rquickjs::Error::Unknown)?;
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or(rquickjs::Error::Unknown)?;
        let id = state.next_id;
        state
            .pending
            .insert(id, ModelSettlement { resolve, reject });
        Ok(id)
    }

    fn abandon(&self, id: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.pending.remove(&id);
        }
    }

    fn settle(&self, ctx: &Ctx<'_>, id: u64, encoded: Option<String>) -> rquickjs::Result<()> {
        let settlement = self
            .state
            .lock()
            .map_err(|_| rquickjs::Error::Unknown)?
            .pending
            .remove(&id)
            .ok_or(rquickjs::Error::Unknown)?;
        match encoded {
            Some(encoded) => settlement.resolve.restore(ctx)?.call((encoded,)),
            None => settlement.reject.restore(ctx)?.call(()),
        }
    }
}

/// Closed loader failures. Source text and thrown values never enter the error surface.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum RealmError {
    #[error("artifact identity validation failed")]
    Identity,
    #[error("artifact declares an invalid export name")]
    InvalidExport,
    #[error("artifact declares a duplicate export name")]
    DuplicateExport,
    #[error("artifact export collides with a model global")]
    ExportCollision,
    #[error("artifact initialization failed")]
    Initialization,
    #[error("trusted private skill library compilation failed")]
    PrivateLibraryCompilation,
    #[error("trusted private skill library bytecode load failed")]
    PrivateLibraryBytecodeLoad,
    #[error("trusted private skill library module evaluation failed")]
    PrivateLibraryModuleEvaluation,
    #[error("trusted private skill library export lookup failed")]
    PrivateLibraryExportLookup,
    #[error("trusted private skill library factory execution failed")]
    PrivateLibraryFactoryExecution,
    #[error("artifact initialization scheduled pending jobs")]
    PendingInitializationJobs,
    #[error("artifact does not define every declared export as a function")]
    MissingExport,
    #[error("artifact wrapper installation failed")]
    WrapperInstallation,
}

/// Metadata proving which immutable artifact was installed into the model context.
#[derive(Debug)]
pub(crate) struct LoadedArtifact {
    artifact_id: String,
    exports: Vec<String>,
    dispatcher_resources: Vec<Arc<Mutex<Option<DispatcherResources>>>>,
}

impl LoadedArtifact {
    pub(crate) fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    pub(crate) fn exports(&self) -> &[String] {
        &self.exports
    }
}

impl Drop for LoadedArtifact {
    fn drop(&mut self) {
        for resources in &self.dispatcher_resources {
            if let Ok(mut resources) = resources.lock() {
                resources.take();
            }
        }
    }
}

#[derive(Clone, Debug)]
struct DispatcherResources {
    wrapper: Persistent<Function<'static>>,
    terminal_host: Persistent<Function<'static>>,
    terminal_wrapper: Persistent<Function<'static>>,
}

/// Load one identity-v2 artifact into a new private context and install exact frozen wrappers.
///
/// The caller must invoke this before model source evaluation. Any error rejects the whole
/// request; in particular, a pending initialization job is intentionally not drained because
/// running it would execute stored source after the loader has rejected the artifact.
pub(crate) fn load_artifact(
    runtime: &Runtime,
    model_context: &Context,
    artifact: &SkillArtifact,
) -> Result<LoadedArtifact, RealmError> {
    load_artifact_internal(runtime, model_context, artifact, None, None, None)
}

/// Load an ABI-v2 artifact whose wrappers inject a fresh, revocable invocation capability.
pub(crate) fn load_artifact_with_capabilities(
    runtime: &Runtime,
    model_context: &Context,
    artifact: &SkillArtifact,
    capabilities: InvocationCapabilityRuntime,
) -> Result<LoadedArtifact, RealmError> {
    load_artifact_internal(
        runtime,
        model_context,
        artifact,
        Some(Arc::new(capabilities)),
        None,
        None,
    )
}

/// Install Rust-owned model dispatchers backed by fresh parent authority for every call.
pub(crate) fn load_artifact_with_bound_exports(
    runtime: &Runtime,
    model_context: &Context,
    artifact: &SkillArtifact,
    capabilities: InvocationCapabilityRuntime,
    bindings: HashMap<String, BoundExportInvocation>,
) -> Result<LoadedArtifact, RealmError> {
    load_artifact_internal(
        runtime,
        model_context,
        artifact,
        Some(Arc::new(capabilities)),
        Some(Arc::new(bindings)),
        None,
    )
}

/// Verification-only mutation entry that keeps the production loader path intact.
///
/// The artifact is fully validated and its declared namespace is resolved before the selected
/// bridge target is replaced. Production callers always pass through the same internal loader
/// with no mutation.
pub(crate) fn load_artifact_with_bound_exports_for_verification(
    runtime: &Runtime,
    model_context: &Context,
    artifact: &SkillArtifact,
    capabilities: InvocationCapabilityRuntime,
    bindings: HashMap<String, BoundExportInvocation>,
    mutated_export: Option<&str>,
) -> Result<LoadedArtifact, RealmError> {
    load_artifact_internal(
        runtime,
        model_context,
        artifact,
        Some(Arc::new(capabilities)),
        Some(Arc::new(bindings)),
        mutated_export,
    )
}

/// Call an installed model wrapper under one exact, opaque invocation binding.
///
/// The guard is installed immediately around `Function::call`. Wrapper statement one claims the
/// handle before argument encoding can execute model-controlled proxy traps or re-enter a wrapper.
pub(crate) fn call_export_with_capability<'js, A, R>(
    ctx: &Ctx<'js>,
    export_name: &str,
    capabilities: &InvocationCapabilityRuntime,
    handle: PreparedInvocationHandle,
    arguments: A,
) -> rquickjs::Result<R>
where
    A: IntoArgs<'js>,
    R: FromJs<'js>,
{
    let wrapper: Function = ctx.globals().get(export_name)?;
    call_function_with_capability(&wrapper, capabilities, handle, arguments)
}

fn call_function_with_capability<'js, A, R>(
    wrapper: &Function<'js>,
    capabilities: &InvocationCapabilityRuntime,
    handle: PreparedInvocationHandle,
    arguments: A,
) -> rquickjs::Result<R>
where
    A: IntoArgs<'js>,
    R: FromJs<'js>,
{
    let _binding = capabilities
        .bind(handle)
        .map_err(|_| rquickjs::Error::Unknown)?;
    wrapper.call(arguments)
}

fn call_function_with_capability_args<'js, R>(
    wrapper: &Function<'js>,
    capabilities: &InvocationCapabilityRuntime,
    handle: PreparedInvocationHandle,
    arguments: Vec<Value<'js>>,
) -> rquickjs::Result<R>
where
    R: FromJs<'js>,
{
    let _binding = capabilities
        .bind(handle)
        .map_err(|_| rquickjs::Error::Unknown)?;
    let mut args = Args::new(wrapper.ctx().clone(), arguments.len());
    args.push_args(arguments)?;
    wrapper.call_arg(args)
}

// QuickJS values are confined to this request-local worker thread. `Arc` is used
// to share ownership with capability callbacks, not to cross thread boundaries.
#[allow(clippy::arc_with_non_send_sync)]
fn load_artifact_internal(
    runtime: &Runtime,
    model_context: &Context,
    artifact: &SkillArtifact,
    capabilities: Option<Arc<InvocationCapabilityRuntime>>,
    bound_exports: Option<Arc<HashMap<String, BoundExportInvocation>>>,
    mutated_export: Option<&str>,
) -> Result<LoadedArtifact, RealmError> {
    artifact
        .verify_identity()
        .map_err(|_| RealmError::Identity)?;
    validate_export_names(artifact)?;
    reject_model_collisions(model_context, artifact)?;
    let settlements = capabilities
        .as_ref()
        .map(|_| Arc::new(ModelSettlementRegistry::default()));

    let private_context = Context::full(runtime).map_err(|_| RealmError::Initialization)?;
    let private_skill_library = if artifact_uses_ajv(artifact) {
        Some(private_skill_library_bytecode().ok_or(RealmError::PrivateLibraryCompilation)?)
    } else {
        None
    };
    let (bridge_factory, private_encoder) = private_context
        .with(|ctx| {
            // Capture every boundary primitive before stored source can replace a global.
            let bridge_factory: Function = ctx.eval(if capabilities.is_some() {
                CAPABILITY_BRIDGE_FACTORY_SOURCE
            } else {
                BRIDGE_FACTORY_SOURCE
            })?;
            let encoder: Function = ctx.eval(STRICT_CLONE_SOURCE)?;
            Ok::<_, rquickjs::Error>((
                Persistent::save(&ctx, bridge_factory),
                Persistent::save(&ctx, encoder),
            ))
        })
        .map_err(|_| RealmError::Initialization)?;
    if let Some(bytecode) = private_skill_library {
        let bridge = build_private_skill_library_bridge(runtime, bytecode)?;
        private_context.with(|ctx| install_private_skill_library_facade(&ctx, bridge))?;
    }
    private_context
        .with(|ctx| {
            ctx.eval::<(), _>(SKILL_REALM_HARDENING_JS)?;

            let mut options = EvalOptions::default();
            options.filename = Some(format!("skill-{}.js", artifact.id));
            // Evaluate the artifact itself as a Script. Wrapping it in a generated function would
            // change the accepted grammar (notably top-level return/import handling) and would
            // make source-created namespace objects part of the trusted loader boundary.
            let _: Value =
                ctx.eval_with_options(private_skill_source(artifact).as_bytes(), options)?;
            Ok::<_, rquickjs::Error>(())
        })
        .map_err(|_| RealmError::Initialization)?;

    if runtime.is_job_pending() {
        return Err(RealmError::PendingInitializationJobs);
    }

    // Resolve declared lexical/global bindings into a loader-owned namespace. Its properties are
    // ordinary own data properties, so later bridge construction cannot dispatch a getter or a
    // source-created namespace Proxy.
    let namespace = private_context
        .with(|ctx| {
            let namespace = Object::new_proto(ctx.clone(), None)?;
            for export in &artifact.exports {
                let original: Function = ctx.eval(export.name.as_bytes())?;
                namespace.prop(export.name.as_str(), Property::from(original).enumerable())?;
            }
            Ok::<_, rquickjs::Error>(Persistent::save(&ctx, namespace))
        })
        .map_err(|_| RealmError::MissingExport)?;

    if runtime.is_job_pending() {
        return Err(RealmError::PendingInitializationJobs);
    }

    let bridges = private_context
        .with(|ctx| {
            let namespace = namespace.restore(&ctx)?;
            let bridge_factory = bridge_factory.restore(&ctx)?;
            let private_encoder = private_encoder.restore(&ctx)?;
            artifact
                .exports
                .iter()
                .map(|export| {
                    let original: Function = if mutated_export == Some(export.name.as_str()) {
                        ctx.eval("(() => { throw 0; })")?
                    } else {
                        namespace.get(export.name.as_str())?
                    };
                    let bridge: Function = if let Some(capabilities) = capabilities.as_ref() {
                        let settlements = settlements
                            .as_ref()
                            .expect("capability loader has settlement registry");
                        let dispatch_capabilities = capabilities.clone();
                        let dispatch = Function::new(
                            ctx.clone(),
                            move |token: u64, method: String, arguments: String| {
                                let operation = HostCapability::from_token(&method)
                                    .ok_or(rquickjs::Error::Unknown)?;
                                dispatch_capabilities
                                    .dispatch(token, operation, &arguments)
                                    .map_err(|_| rquickjs::Error::Unknown)
                            },
                        )?;
                        let finish_capabilities = capabilities.clone();
                        let finish = Function::new(ctx.clone(), move |token: u64| {
                            finish_capabilities.finish(token);
                        })?;
                        let success_settlements = settlements.clone();
                        let settle_success = Function::new(
                            ctx.clone(),
                            move |ctx: Ctx<'_>, settlement_id: u64, encoded: String| {
                                success_settlements.settle(&ctx, settlement_id, Some(encoded))
                            },
                        )?;
                        let failure_settlements = settlements.clone();
                        let settle_failure =
                            Function::new(ctx.clone(), move |ctx: Ctx<'_>, settlement_id: u64| {
                                failure_settlements.settle(&ctx, settlement_id, None)
                            })?;
                        let methods = artifact
                            .capability
                            .grants
                            .iter()
                            .map(|scope| scope.capability().as_token())
                            .collect::<Vec<_>>();
                        let encoded_methods = serde_json::to_string(&methods)
                            .map_err(|_| rquickjs::Error::Unknown)?;
                        let methods = ctx.json_parse(encoded_methods)?;
                        let capability_factory: Function = bridge_factory.call((
                            original,
                            private_encoder.clone(),
                            dispatch,
                            finish,
                        ))?;
                        capability_factory.call((settle_success, settle_failure, methods))?
                    } else {
                        bridge_factory.call((original, private_encoder.clone()))?
                    };
                    Ok(Persistent::save(&ctx, bridge))
                })
                .collect::<rquickjs::Result<Vec<_>>>()
        })
        .map_err(|_| RealmError::MissingExport)?;

    if runtime.is_job_pending() {
        return Err(RealmError::PendingInitializationJobs);
    }

    let (wrappers, dispatcher_resources) = build_model_wrappers(
        model_context,
        artifact,
        bridges,
        capabilities,
        settlements,
        bound_exports,
    )?;
    if runtime.is_job_pending() {
        return Err(RealmError::PendingInitializationJobs);
    }
    publish_model_wrappers(model_context, wrappers)?;
    Ok(LoadedArtifact {
        artifact_id: artifact.id.clone(),
        exports: artifact
            .exports
            .iter()
            .map(|export| export.name.clone())
            .collect(),
        dispatcher_resources,
    })
}

fn validate_export_names(artifact: &SkillArtifact) -> Result<(), RealmError> {
    let mut names = HashSet::with_capacity(artifact.exports.len());
    for export in &artifact.exports {
        let mut characters = export.name.chars();
        let valid_start = characters
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
        if !valid_start
            || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
        {
            return Err(RealmError::InvalidExport);
        }
        if !names.insert(export.name.as_str()) {
            return Err(RealmError::DuplicateExport);
        }
    }
    Ok(())
}

fn reject_model_collisions(
    model_context: &Context,
    artifact: &SkillArtifact,
) -> Result<(), RealmError> {
    model_context.with(|ctx| {
        for export in &artifact.exports {
            if ctx
                .globals()
                .contains_key(export.name.as_str())
                .map_err(|_| RealmError::ExportCollision)?
            {
                return Err(RealmError::ExportCollision);
            }
        }
        Ok(())
    })
}

type InstalledWrapper = (String, Persistent<Function<'static>>);
type DispatcherResourceOwner = Arc<Mutex<Option<DispatcherResources>>>;

fn build_model_wrappers(
    model_context: &Context,
    artifact: &SkillArtifact,
    bridges: Vec<Persistent<Function<'static>>>,
    capabilities: Option<Arc<InvocationCapabilityRuntime>>,
    settlements: Option<Arc<ModelSettlementRegistry>>,
    bound_exports: Option<Arc<HashMap<String, BoundExportInvocation>>>,
) -> Result<(Vec<InstalledWrapper>, Vec<DispatcherResourceOwner>), RealmError> {
    model_context
        .with(|ctx| {
            // These closures are captured before model source runs, so model prototype/global
            // poisoning cannot change the clone or wrapper contract.
            let model_encoder: Function = ctx.eval(STRICT_CLONE_SOURCE)?;
            let wrapper_factory: Function = ctx.eval(if settlements.is_some() {
                MODEL_WRAPPER_FACTORY_SOURCE
            } else {
                PURE_MODEL_WRAPPER_FACTORY_SOURCE
            })?;
            let model_encoder = Persistent::save(&ctx, model_encoder);
            let settlement_functions = if let Some(settlements) = settlements.as_ref() {
                let prepare_settlements = settlements.clone();
                let prepare = Function::new(
                    ctx.clone(),
                    move |resolve: Persistent<Function<'static>>,
                          reject: Persistent<Function<'static>>| {
                        prepare_settlements.prepare(resolve, reject)
                    },
                )?;
                let abandon_settlements = settlements.clone();
                let abandon = Function::new(ctx.clone(), move |settlement_id: u64| {
                    abandon_settlements.abandon(settlement_id);
                })?;
                Some((prepare, abandon))
            } else {
                None
            };

            if let Some(bindings) = bound_exports.as_ref()
                && (bindings.len() != artifact.exports.len()
                    || artifact
                        .exports
                        .iter()
                        .any(|export| !bindings.contains_key(&export.name)))
            {
                return Err(rquickjs::Error::Unknown);
            }

            let mut dispatcher_resources = Vec::new();
            let wrappers = artifact
                .exports
                .iter()
                .zip(bridges)
                .map(|(export, bridge)| {
                    // Restoring in a sibling context is the A02-proven bridge. The bridge itself
                    // accepts and returns only bounded encoded strings; model arguments and skill
                    // results are never passed to the original function by reference.
                    let invoke = bridge.restore(&ctx)?;
                    let wrapper: Function = if let Some((prepare, abandon)) = &settlement_functions
                    {
                        let capabilities = capabilities
                            .as_ref()
                            .expect("settlement wrappers have invocation capabilities");
                        let claim_capabilities = capabilities.clone();
                        let artifact_id = artifact.id.clone();
                        let export_name = export.name.clone();
                        let manifest = artifact.capability.clone();
                        let claim = Function::new(ctx.clone(), move || {
                            claim_capabilities
                                .claim_bound(&artifact_id, &export_name, &manifest)
                                .map_err(|_| rquickjs::Error::Unknown)
                        })?;
                        let revoke_capabilities = capabilities.clone();
                        let revoke = Function::new(ctx.clone(), move |token: u64| {
                            revoke_capabilities.finish(token);
                        })?;
                        wrapper_factory.call((
                            invoke,
                            model_encoder.clone().restore(&ctx)?,
                            claim,
                            revoke,
                            prepare.clone(),
                            abandon.clone(),
                        ))?
                    } else {
                        // Pure A16 wrappers never accept promises and retain their smaller bridge.
                        wrapper_factory.call((invoke, model_encoder.clone().restore(&ctx)?))?
                    };
                    let wrapper = if let Some(bindings) = bound_exports.as_ref() {
                        let binding = bindings
                            .get(&export.name)
                            .ok_or(rquickjs::Error::Unknown)?
                            .clone();
                        let (dispatcher, resources) = build_bound_dispatcher(
                            &ctx,
                            wrapper,
                            capabilities
                                .as_ref()
                                .ok_or(rquickjs::Error::Unknown)?
                                .clone(),
                            binding,
                        )?;
                        dispatcher_resources.push(resources);
                        dispatcher
                    } else {
                        wrapper
                    };
                    Ok((export.name.clone(), Persistent::save(&ctx, wrapper)))
                })
                .collect::<rquickjs::Result<Vec<_>>>()?;
            Ok((wrappers, dispatcher_resources))
        })
        .map_err(|_| RealmError::WrapperInstallation)
}

// The dispatcher and its persistent QuickJS values never leave the fresh realm's
// worker thread; shared ownership only ties callback and teardown lifetimes.
#[allow(clippy::arc_with_non_send_sync)]
fn build_bound_dispatcher<'js>(
    ctx: &Ctx<'js>,
    wrapper: Function<'js>,
    capabilities: Arc<InvocationCapabilityRuntime>,
    binding: BoundExportInvocation,
) -> rquickjs::Result<(Function<'js>, Arc<Mutex<Option<DispatcherResources>>>)> {
    let private_wrapper = Persistent::save(ctx, wrapper);
    let next_call_ordinal = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let authorize = binding.authorize;
    let start = binding.on_start;
    let terminal = binding.on_terminal;
    let terminal_host = {
        let terminal = terminal.clone();
        Function::new(ctx.clone(), move |invocation_id: String, success: bool| {
            terminal(invocation_id, success).map_err(|_| rquickjs::Error::Unknown)
        })?
    };
    let terminal_host = Persistent::save(ctx, terminal_host);
    let terminal_wrapper: Function = ctx.eval(TERMINAL_WRAPPER_SOURCE)?;
    let terminal_wrapper = Persistent::save(ctx, terminal_wrapper);
    let resources = Arc::new(Mutex::new(Some(DispatcherResources {
        wrapper: private_wrapper,
        terminal_host,
        terminal_wrapper,
    })));
    let dispatch_resources = resources.clone();
    let dispatcher = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              Rest(arguments): Rest<Value<'js>>|
              -> rquickjs::Result<Persistent<Value<'static>>> {
            // Each entry asks the parent for the next exact ordinal. The returned opaque handle
            // remains one-shot; only the export binding itself is reusable.
            let call_ordinal = next_call_ordinal
                .fetch_update(
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                    |ordinal| ordinal.checked_add(1),
                )
                .map_err(|_| rquickjs::Error::Unknown)?;
            let (handle, invocation_id) =
                authorize(call_ordinal).map_err(|_| rquickjs::Error::Unknown)?;
            start(invocation_id.clone(), argument_shape(&arguments))
                .map_err(|_| rquickjs::Error::Unknown)?;
            let resources = dispatch_resources
                .lock()
                .map_err(|_| rquickjs::Error::Unknown)?
                .clone()
                .ok_or(rquickjs::Error::Unknown)?;
            let wrapper = resources.wrapper.restore(&ctx)?;
            let result: Value = match call_function_with_capability_args(
                &wrapper,
                &capabilities,
                handle,
                arguments,
            ) {
                Ok(result) => result,
                Err(error) => {
                    let _ = terminal(invocation_id, false);
                    return Err(error);
                }
            };
            let observed: Value = resources.terminal_wrapper.restore(&ctx)?.call((
                result,
                invocation_id,
                resources.terminal_host.restore(&ctx)?,
            ))?;
            Ok(Persistent::save(&ctx, observed))
        },
    )?;
    Ok((dispatcher, resources))
}

fn argument_shape(arguments: &[Value<'_>]) -> String {
    let types = arguments
        .iter()
        .map(|value| {
            if value.is_null() {
                "null"
            } else if value.as_array().is_some() {
                "array"
            } else {
                match value.type_name() {
                    "bool" => "boolean",
                    "int" | "float" => "number",
                    other => other,
                }
            }
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&serde_json::json!({
        "argc": arguments.len(),
        "types": types,
    }))
    .unwrap_or_else(|_| r#"{"truncated":true}"#.to_string())
}

fn publish_model_wrappers(
    model_context: &Context,
    wrappers: Vec<(String, Persistent<Function<'static>>)>,
) -> Result<(), RealmError> {
    model_context
        .with(|ctx| {
            let globals = ctx.globals();

            // Recheck immediately before publication. All wrappers have already been built, and
            // duplicate declarations were rejected before source evaluation.
            for (name, _) in &wrappers {
                if globals.contains_key(name.as_str())? {
                    return Err(rquickjs::Error::Unknown);
                }
            }

            let wrappers = wrappers
                .into_iter()
                .map(|(name, wrapper)| Ok((name, wrapper.restore(&ctx)?)))
                .collect::<rquickjs::Result<Vec<_>>>()?;
            // Every semantic failure mode has been checked before the first mutation. Property
            // installation uses final non-configurable descriptors so model code cannot delete or
            // replace a learned-skill binding. An unexpected engine resource failure rejects the
            // whole disposable request/runtime, as required by `load_artifact`'s caller contract.
            for (name, wrapper) in wrappers {
                globals.prop(name.as_str(), Property::from(wrapper).enumerable())?;
            }
            Ok::<_, rquickjs::Error>(())
        })
        .map_err(|_| RealmError::WrapperInstallation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::js::skills::{CapabilityManifest, SkillExport};

    #[test]
    fn invalid_identifier_is_rejected_before_source_generation() {
        let runtime = Runtime::new().unwrap();
        let model = Context::full(&runtime).unwrap();
        let artifact = SkillArtifact::new(
            "throw new Error('must not execute')".to_string(),
            "invalid export fixture".to_string(),
            Vec::new(),
            vec![SkillExport {
                name: "valid};globalThis.escape=1;//".to_string(),
                signature: "()".to_string(),
            }],
            vec!["true".to_string()],
            CapabilityManifest::pure(),
        )
        .unwrap();

        assert!(matches!(
            load_artifact(&runtime, &model, &artifact),
            Err(RealmError::InvalidExport)
        ));
    }
}
