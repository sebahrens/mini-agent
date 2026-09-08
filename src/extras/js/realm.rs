//! Pure learned-skill realm loading for the Phase 6 worker.
//!
//! A loader invocation creates one private QuickJS context for one immutable identity-v2
//! artifact. Stored source sees no effect, proposal, or module globals. The only model-visible
//! values are frozen wrappers; wrapper arguments and results cross contexts as bounded strict
//! JSON strings. Invocation capability construction is deliberately owned by Phase 6 A17.

use rquickjs::context::EvalOptions;
use rquickjs::function::{Args, Rest};
// `IntoArgs` is only needed by the cfg(test) by-name wrapper caller below.
#[cfg(test)]
use rquickjs::function::IntoArgs;
use rquickjs::object::Property;
use rquickjs::{
    Context, Ctx, Exception, FromJs, Function, Module, Object, Persistent, Runtime, Value,
    WriteOptions, qjs,
};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::slice;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use thiserror::Error;

use super::skills::capability::{
    CapabilityError, InvocationCapabilityRuntime, PreparedInvocationHandle, effect_error_code_token,
};
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

// AJV's compiler intentionally uses `Function` to turn schemas into validators. The trusted
// loader initializes it before hardening and routes those constructor calls through native
// QuickJS eval for Windows portability. Stored source receives only a frozen JSON facade, so
// neither the compiler shim nor AJV's mutable instance is reachable. Skills that do not validate
// JSON retain the existing resource envelope.
const PRIVATE_SKILL_LIBRARY_MODULE_NAME: &str = "mini-agent:private-skill-library";
const PRIVATE_SKILL_LIBRARY_FACTORY_SOURCE: &str = concat!(
    r#"(function (Function, validateFallback) {
const initialize = function () {
const self = globalThis;
"#,
    include_str!("vendor/ajv.min.js"),
    r#"
const AjvConstructor = globalThis.ajv7.default || globalThis.ajv7;
try {
    const instance = new AjvConstructor({
        allErrors: false, strict: false, validateSchema: false, verbose: false, messages: false,
        code: {es5: true, optimize: false}, logger: false, meta: false
    });
    const freeze = Object.freeze;
    const parse = JSON.parse;
    const stringify = JSON.stringify;
    const render = String;
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
            if (validateFallback !== undefined) {
                const result = parse(validateFallback(stringify([schema, data])));
                validationErrors = freezeErrors(result[1]);
                return result[0] === true;
            }
            const valid = instance.validate(schema, data);
            validationErrors = freezeErrors(
                valid ? null : parse(stringify(instance.errors))
            );
            try { instance.removeSchema(); } catch (_) {}
            return valid;
        } catch (error) {
            try { instance.removeSchema(); } catch (_) {}
            let keyword = 'schemaExecution';
            if (error instanceof RangeError) {
                keyword = render(error).toLowerCase().includes('stack')
                    ? 'schemaExecutionStack'
                    : 'schemaExecutionRange';
            }
            else if (error instanceof ReferenceError) keyword = 'schemaExecutionReference';
            else if (error instanceof TypeError) keyword = 'schemaExecutionType';
            else if (error instanceof SyntaxError) keyword = 'schemaExecutionSyntax';
            validationErrors = freezeErrors([{
                instancePath: '', schemaPath: '', keyword, params: {}
            }]);
            return false;
        }
    });
    const api = Object.create(null);
    Object.defineProperty(api, 'validate', {
    value: validate, enumerable: true, writable: false, configurable: false
    });
    Object.defineProperty(api, 'errors', {
    get: freeze(() => validationErrors), enumerable: true, configurable: false
    });
    freeze(api);
    Object.defineProperty(globalThis, 'Ajv', {
    value: api, enumerable: false, writable: false, configurable: false
    });
} finally {
    delete globalThis.ajv7;
}
};
initialize();
})
"#,
);

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
    Rest(parts): Rest<String>,
) -> rquickjs::Result<Function<'js>> {
    let (body, parameters) = parts.split_last().ok_or(rquickjs::Error::Unknown)?;
    // AJV supplies the same parameter/body strings it would pass to the standard Function
    // constructor. This trusted shim changes only the QuickJS API used to compile them.
    let source = format!("(function({}) {{\n{}\n}})", parameters.join(","), body);
    ctx.eval(source)
}

#[cfg(windows)]
fn validate_schema_without_codegen(encoded: String) -> String {
    let generic = || {
        r#"[false,[{"instancePath":"","schemaPath":"","keyword":"schema","params":{}}]]"#.to_owned()
    };
    let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(&encoded) else {
        return generic();
    };
    let [schema, data]: [serde_json::Value; 2] = match values.try_into() {
        Ok(values) => values,
        Err(_) => return generic(),
    };
    let Ok(validator) = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft7)
        .build(&schema)
    else {
        return generic();
    };
    let Some(error) = validator.iter_errors(&data).next() else {
        return "[true,null]".to_owned();
    };
    let instance_path = error.instance_path().to_string();
    let schema_location = error.schema_path().to_string();
    let keyword = schema_location
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("schema");
    serde_json::to_string(&serde_json::json!([false, [{
        "instancePath": instance_path,
        "schemaPath": format!("#{schema_location}"),
        "keyword": keyword,
        "params": {},
    }]]))
    .unwrap_or_else(|_| generic())
}

#[allow(unsafe_code)]
fn install_private_skill_library<'js>(ctx: &Ctx<'js>, bytecode: &[u8]) -> Result<(), RealmError> {
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
    let function_constructor = Function::new(ctx.clone(), compile_private_library_function)
        .map_err(|_| RealmError::PrivateLibraryExportLookup)?
        .with_constructor(true);
    #[cfg(windows)]
    {
        let fallback = Function::new(ctx.clone(), validate_schema_without_codegen)
            .map_err(|_| RealmError::PrivateLibraryExportLookup)?;
        install
            .call::<_, ()>((function_constructor, fallback))
            .map_err(|_| RealmError::PrivateLibraryFactoryExecution)
    }
    #[cfg(not(windows))]
    install
        .call::<_, ()>((function_constructor,))
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

const REALM_BOOTSTRAP_MODULE_NAME: &str = "mini-agent:realm-bootstrap";
static REALM_BOOTSTRAP_BYTECODE: OnceLock<Option<Vec<u8>>> = OnceLock::new();

fn realm_bootstrap_source() -> String {
    format!(
        "export const strictClone = {STRICT_CLONE_SOURCE};\n\
         export const pureBridgeFactory = {BRIDGE_FACTORY_SOURCE};\n\
         export const capabilityBridgeFactory = {CAPABILITY_BRIDGE_FACTORY_SOURCE};\n\
         export const pureModelWrapperFactory = {PURE_MODEL_WRAPPER_FACTORY_SOURCE};\n\
         export const modelWrapperFactory = {MODEL_WRAPPER_FACTORY_SOURCE};\n\
         export const terminalWrapper = {TERMINAL_WRAPPER_SOURCE};"
    )
}

fn compile_realm_bootstrap_bytecode() -> rquickjs::Result<Vec<u8>> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + STEP_TIMEOUT;
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    let context = Context::full(&runtime)?;
    context.with(|ctx| {
        Module::declare(ctx, REALM_BOOTSTRAP_MODULE_NAME, realm_bootstrap_source())?
            .write(WriteOptions::default())
    })
}

fn realm_bootstrap_bytecode() -> Option<&'static [u8]> {
    REALM_BOOTSTRAP_BYTECODE
        .get_or_init(|| compile_realm_bootstrap_bytecode().ok())
        .as_deref()
}

struct RealmBootstrapFunctions {
    strict_clone: Persistent<Function<'static>>,
    pure_bridge_factory: Persistent<Function<'static>>,
    capability_bridge_factory: Persistent<Function<'static>>,
    pure_model_wrapper_factory: Persistent<Function<'static>>,
    model_wrapper_factory: Persistent<Function<'static>>,
    terminal_wrapper: Persistent<Function<'static>>,
}

#[allow(unsafe_code)]
fn load_realm_bootstrap_functions(
    context: &Context,
) -> Result<RealmBootstrapFunctions, RealmError> {
    let bytecode = realm_bootstrap_bytecode().ok_or(RealmError::Initialization)?;
    context
        .with(|ctx| {
            // SAFETY: these process-local bytes are compiled once from the trusted constants
            // above by the same linked QuickJS ABI and never cross disk, IPC, or model input.
            let module = unsafe { Module::load(ctx.clone(), bytecode)? };
            let (module, evaluation) = module.eval()?;
            evaluation.finish::<()>()?;
            let strict_clone: Function = module.get("strictClone")?;
            let pure_bridge_factory: Function = module.get("pureBridgeFactory")?;
            let capability_bridge_factory: Function = module.get("capabilityBridgeFactory")?;
            let pure_model_wrapper_factory: Function = module.get("pureModelWrapperFactory")?;
            let model_wrapper_factory: Function = module.get("modelWrapperFactory")?;
            let terminal_wrapper: Function = module.get("terminalWrapper")?;
            Ok::<_, rquickjs::Error>(RealmBootstrapFunctions {
                strict_clone: Persistent::save(&ctx, strict_clone),
                pure_bridge_factory: Persistent::save(&ctx, pure_bridge_factory),
                capability_bridge_factory: Persistent::save(&ctx, capability_bridge_factory),
                pure_model_wrapper_factory: Persistent::save(&ctx, pure_model_wrapper_factory),
                model_wrapper_factory: Persistent::save(&ctx, model_wrapper_factory),
                terminal_wrapper: Persistent::save(&ctx, terminal_wrapper),
            })
        })
        .map_err(|_| RealmError::Initialization)
}

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

/// Opaque binding between one full artifact identity and its same-process QuickJS bytecode.
pub(crate) struct CompiledArtifactBytecode {
    artifact_id: String,
    bytes: Vec<u8>,
}

impl CompiledArtifactBytecode {
    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn for_artifact(&self, artifact: &SkillArtifact) -> Result<&[u8], RealmError> {
        (self.artifact_id == artifact.id)
            .then_some(self.bytes.as_slice())
            .ok_or(RealmError::Identity)
    }
}

/// Compile an identity-checked artifact as global Script bytecode without executing it.
///
/// A disposable compiler runtime keeps untrusted parser allocations inside the same memory,
/// stack, and deadline envelope used by execution. The resulting bytes are process-local and are
/// never accepted from a wire or persistence boundary.
pub(crate) fn compile_artifact_bytecode(
    artifact: &SkillArtifact,
) -> Result<CompiledArtifactBytecode, RealmError> {
    artifact
        .verify_identity()
        .map_err(|_| RealmError::Identity)?;
    validate_export_names(artifact)?;
    let runtime = Runtime::new().map_err(|_| RealmError::Initialization)?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + STEP_TIMEOUT;
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    let context = Context::full(&runtime).map_err(|_| RealmError::Initialization)?;
    let bytes = context.with(|ctx| compile_global_bytecode(&ctx, artifact))?;
    Ok(CompiledArtifactBytecode {
        artifact_id: artifact.id.clone(),
        bytes,
    })
}

#[allow(unsafe_code)]
fn compile_global_bytecode(ctx: &Ctx<'_>, artifact: &SkillArtifact) -> Result<Vec<u8>, RealmError> {
    let source =
        CString::new(private_skill_source(artifact)).map_err(|_| RealmError::Initialization)?;
    let filename = CString::new(format!("skill-{}.js", artifact.id))
        .map_err(|_| RealmError::Initialization)?;
    let source_len = artifact
        .source
        .len()
        .try_into()
        .map_err(|_| RealmError::Initialization)?;
    // SAFETY: both pointers are live for the call and lengths describe the exact immutable source.
    // COMPILE_ONLY returns an owned QuickJS function object without evaluating stored code.
    let raw = unsafe {
        qjs::JS_Eval(
            ctx.as_raw().as_ptr(),
            source.as_ptr(),
            source_len,
            filename.as_ptr(),
            (qjs::JS_EVAL_TYPE_GLOBAL | qjs::JS_EVAL_FLAG_STRICT | qjs::JS_EVAL_FLAG_COMPILE_ONLY)
                as i32,
        )
    };
    if unsafe { qjs::JS_IsException(raw) } {
        let _ = ctx.catch();
        return Err(RealmError::Initialization);
    }
    // SAFETY: `raw` is an owned value from this exact context. Wrapping it transfers ownership to
    // `Value`, whose drop releases it after serialization.
    let compiled = unsafe { Value::from_raw(ctx.clone(), raw) };
    let mut length = MaybeUninit::uninit();
    // SAFETY: the compiled function belongs to `ctx`; QuickJS allocates the returned byte buffer.
    let bytes = unsafe {
        qjs::JS_WriteObject(
            ctx.as_raw().as_ptr(),
            length.as_mut_ptr(),
            compiled.as_raw(),
            qjs::JS_WRITE_OBJ_BYTECODE as i32,
        )
    };
    if bytes.is_null() {
        let _ = ctx.catch();
        return Err(RealmError::Initialization);
    }
    // SAFETY: QuickJS returned `length` initialized bytes and retains ownership until `js_free`.
    let length = unsafe { length.assume_init() };
    let length = match usize::try_from(length) {
        Ok(length) => length,
        Err(_) => {
            // SAFETY: conversion failure does not change ownership of the QuickJS buffer.
            unsafe { qjs::js_free(ctx.as_raw().as_ptr(), bytes.cast()) };
            return Err(RealmError::Initialization);
        }
    };
    let output = unsafe { slice::from_raw_parts(bytes, length) }.to_vec();
    // SAFETY: this releases the buffer with the same context allocator that produced it.
    unsafe { qjs::js_free(ctx.as_raw().as_ptr(), bytes.cast()) };
    Ok(output)
}

#[allow(unsafe_code)]
fn evaluate_global_bytecode(ctx: &Ctx<'_>, bytecode: &[u8]) -> rquickjs::Result<()> {
    let bytecode_len = bytecode
        .len()
        .try_into()
        .map_err(|_| rquickjs::Error::Unknown)?;
    // SAFETY: callers supply only same-process bytes produced by `compile_artifact_bytecode` for
    // the identity-checked artifact currently being loaded. The byte slice lives for the call.
    let compiled = unsafe {
        qjs::JS_ReadObject(
            ctx.as_raw().as_ptr(),
            bytecode.as_ptr(),
            bytecode_len,
            qjs::JS_READ_OBJ_BYTECODE as i32,
        )
    };
    if unsafe { qjs::JS_IsException(compiled) } {
        let _ = ctx.catch();
        return Err(rquickjs::Error::Exception);
    }
    // SAFETY: `JS_EvalFunction` consumes the owned object returned by `JS_ReadObject` and returns
    // another owned value in the same context.
    let result = unsafe { qjs::JS_EvalFunction(ctx.as_raw().as_ptr(), compiled) };
    if unsafe { qjs::JS_IsException(result) } {
        let _ = ctx.catch();
        return Err(rquickjs::Error::Exception);
    }
    // SAFETY: the successful result is owned by this context; the wrapper releases it on drop.
    drop(unsafe { Value::from_raw(ctx.clone(), result) });
    Ok(())
}

/// Load one identity-v2 artifact into a new private context and install exact frozen wrappers.
///
/// The caller must invoke this before model source evaluation. Any error rejects the whole
/// request; in particular, a pending initialization job is intentionally not drained because
/// running it would execute stored source after the loader has rejected the artifact.
// Test-only: production always loads through
// `load_artifact_with_bound_exports_bytecode` (or the verification variant).
#[cfg(test)]
pub(crate) fn load_artifact(
    runtime: &Runtime,
    model_context: &Context,
    artifact: &SkillArtifact,
) -> Result<LoadedArtifact, RealmError> {
    load_artifact_internal(runtime, model_context, artifact, None, None, None, None)
}

/// Load an ABI-v2 artifact whose wrappers inject a fresh, revocable invocation capability.
// Test-only: production always loads through
// `load_artifact_with_bound_exports_bytecode` (or the verification variant).
#[cfg(test)]
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
        None,
    )
}

/// Install Rust-owned model dispatchers backed by fresh parent authority for every call.
// Test-only: production always loads through
// `load_artifact_with_bound_exports_bytecode` (or the verification variant).
#[cfg(test)]
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
        None,
    )
}

/// Load production skill source from worker-local bytecode compiled from this exact artifact.
///
/// The bytes never cross IPC or disk and are accepted only after the artifact identity is checked
/// again. Loading them saves parsing and compilation while preserving a fresh runtime and private
/// context for every model step.
pub(crate) fn load_artifact_with_bound_exports_bytecode(
    runtime: &Runtime,
    model_context: &Context,
    artifact: &SkillArtifact,
    bytecode: &CompiledArtifactBytecode,
    capabilities: InvocationCapabilityRuntime,
    bindings: HashMap<String, BoundExportInvocation>,
) -> Result<LoadedArtifact, RealmError> {
    let bytecode = bytecode.for_artifact(artifact)?;
    load_artifact_internal(
        runtime,
        model_context,
        artifact,
        Some(Arc::new(capabilities)),
        Some(Arc::new(bindings)),
        None,
        Some(bytecode),
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
    mutation: Option<(&str, super::protocol::VerificationMutation)>,
) -> Result<LoadedArtifact, RealmError> {
    load_artifact_internal(
        runtime,
        model_context,
        artifact,
        Some(Arc::new(capabilities)),
        Some(Arc::new(bindings)),
        mutation,
        None,
    )
}

/// Call an installed model wrapper under one exact, opaque invocation binding.
///
/// The guard is installed immediately around `Function::call`. Wrapper statement one claims the
/// handle before argument encoding can execute model-controlled proxy traps or re-enter a wrapper.
// Test-only: production dispatches bound exports through
// `call_function_with_capability_args`, not by name off the globals object.
#[cfg(test)]
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

#[cfg(test)]
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
    mutation: Option<(&str, super::protocol::VerificationMutation)>,
    artifact_bytecode: Option<&[u8]>,
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
    let private_bootstrap = load_realm_bootstrap_functions(&private_context)?;
    let (bridge_factory, private_encoder) = private_context
        .with(|ctx| {
            // Capture every boundary primitive before stored source can replace a global.
            let bridge_factory = if capabilities.is_some() {
                private_bootstrap.capability_bridge_factory.restore(&ctx)?
            } else {
                private_bootstrap.pure_bridge_factory.restore(&ctx)?
            };
            let encoder = private_bootstrap.strict_clone.restore(&ctx)?;
            Ok::<_, rquickjs::Error>((
                Persistent::save(&ctx, bridge_factory),
                Persistent::save(&ctx, encoder),
            ))
        })
        .map_err(|_| RealmError::Initialization)?;
    if let Some(bytecode) = private_skill_library {
        private_context.with(|ctx| install_private_skill_library(&ctx, bytecode))?;
    }
    private_context
        .with(|ctx| {
            ctx.eval::<(), _>(SKILL_REALM_HARDENING_JS)?;

            if let Some(bytecode) = artifact_bytecode {
                evaluate_global_bytecode(&ctx, bytecode)?;
            } else {
                let mut options = EvalOptions::default();
                options.filename = Some(format!("skill-{}.js", artifact.id));
                // Evaluate the artifact itself as a Script. Wrapping it in a generated function
                // would change the accepted grammar (notably top-level return/import handling)
                // and would make source-created namespace objects part of the trusted loader
                // boundary.
                let _: Value =
                    ctx.eval_with_options(private_skill_source(artifact).as_bytes(), options)?;
            }
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

    // Mutate the private lexical/global binding when the source declaration is
    // assignable. This makes calls routed through another exported function
    // observe the mutant as well. `const` bindings reject assignment, so the
    // selected public bridge below remains the deterministic fallback.
    if let Some((export_name, mutation_kind)) = mutation {
        let mutant = match mutation_kind {
            super::protocol::VerificationMutation::Throw => "(() => { throw 0; })",
            super::protocol::VerificationMutation::ReturnNull => "(() => null)",
        };
        let assignment = format!("{export_name} = {mutant}; void 0");
        let _ = private_context.with(|ctx| ctx.eval::<(), _>(assignment.as_bytes()));
    }

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
                    let original: Function =
                        if mutation.is_some_and(|(name, _)| name == export.name.as_str()) {
                            match mutation.expect("selected mutation exists").1 {
                                super::protocol::VerificationMutation::Throw => {
                                    ctx.eval("(() => { throw 0; })")?
                                }
                                super::protocol::VerificationMutation::ReturnNull => {
                                    ctx.eval("(() => null)")?
                                }
                            }
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
                            move |ctx: Ctx<'_>, token: u64, method: String, arguments: String| {
                                let operation = HostCapability::from_token(&method)
                                    .ok_or(rquickjs::Error::Unknown)?;
                                dispatch_capabilities
                                    .dispatch(token, operation, &arguments)
                                    .map_err(|error| skill_effect_exception(&ctx, &method, &error))
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
    let bootstrap = load_realm_bootstrap_functions(model_context)?;
    model_context
        .with(|ctx| {
            // These closures are captured before model source runs, so model prototype/global
            // poisoning cannot change the clone or wrapper contract.
            let model_encoder = bootstrap.strict_clone.restore(&ctx)?;
            let wrapper_factory = if settlements.is_some() {
                bootstrap.model_wrapper_factory.restore(&ctx)?
            } else {
                bootstrap.pure_model_wrapper_factory.restore(&ctx)?
            };
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
                            bootstrap.terminal_wrapper.clone(),
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
    terminal_wrapper: Persistent<Function<'static>>,
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

/// Raise the code-bearing exception the learned-skill ABI reports failures with.
///
/// Model-authored effect globals already throw an `Error` carrying a closed
/// `code` property. Collapsing every brokered failure into
/// `rquickjs::Error::Unknown` made `not_found`, `too_large`, `timed_out`,
/// `backend_failure` and a real denial indistinguishable inside skill code, so
/// a skill could not, for example, fall back when a file is missing. The same
/// tokens and the same `code` property are used here.
pub(super) fn skill_effect_exception(
    ctx: &Ctx<'_>,
    method: &str,
    error: &CapabilityError,
) -> rquickjs::Error {
    let code = match error {
        CapabilityError::EffectFailed(code) => effect_error_code_token(*code),
        CapabilityError::InvalidArguments => "invalid_target",
        CapabilityError::DispatchDenied => "denied",
        // `CapabilityError::Denied` only exists in test builds; it belongs to the
        // ambient `CapabilityContext` stack, which production never enters.
        #[cfg(test)]
        CapabilityError::Denied(_) => "capability_denied",
        CapabilityError::Revoked
        | CapabilityError::InvalidInvocation
        | CapabilityError::InvalidAttribution
        | CapabilityError::InvalidManifest(_) => "capability_denied",
    };
    let Ok(exception) = Exception::from_message(ctx.clone(), &format!("{method}: {code}")) else {
        return rquickjs::Error::Unknown;
    };
    if exception.as_object().prop("code", code).is_err() {
        return rquickjs::Error::Unknown;
    }
    exception.throw()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::js::protocol::EffectErrorCode;
    use crate::extras::js::skills::{CapabilityManifest, SkillExport};

    #[test]
    fn a_brokered_effect_failure_reaches_skill_code_with_its_closed_code() {
        let runtime = Runtime::new().expect("runtime");
        let context = Context::full(&runtime).expect("context");
        context.with(|ctx| {
            for (error, expected) in [
                (
                    CapabilityError::EffectFailed(EffectErrorCode::NotFound),
                    "not_found",
                ),
                (
                    CapabilityError::EffectFailed(EffectErrorCode::TooLarge),
                    "too_large",
                ),
                (
                    CapabilityError::EffectFailed(EffectErrorCode::TimedOut),
                    "timed_out",
                ),
                (
                    CapabilityError::EffectFailed(EffectErrorCode::BackendFailure),
                    "backend_failure",
                ),
                (
                    CapabilityError::EffectFailed(EffectErrorCode::Denied),
                    "denied",
                ),
                (CapabilityError::DispatchDenied, "denied"),
                (CapabilityError::Revoked, "capability_denied"),
            ] {
                let raised = skill_effect_exception(&ctx, "read_file", &error);
                assert!(matches!(raised, rquickjs::Error::Exception));
                let thrown = ctx.catch();
                let exception = thrown
                    .as_exception()
                    .expect("the ABI must raise an Error object");
                let code: String = exception
                    .as_object()
                    .get("code")
                    .expect("the exception must carry a closed code");
                assert_eq!(code, expected, "wrong code for {error}");
                assert_eq!(
                    exception.message().expect("exception message"),
                    format!("read_file: {expected}")
                );
            }
        });
    }

    #[test]
    fn trusted_realm_bootstrap_bytecode_loads_repeatedly_without_shared_state() {
        for _ in 0..2 {
            let runtime = Runtime::new().unwrap();
            let context = Context::full(&runtime).unwrap();
            for _ in 0..2 {
                let functions = load_realm_bootstrap_functions(&context).unwrap();
                context
                    .with(|ctx| {
                        let clone = functions.strict_clone.restore(&ctx)?;
                        let value: Object = ctx.eval("({fresh: true})")?;
                        let encoded: String = clone.call((value,))?;
                        assert_eq!(encoded, r#"{"fresh":true}"#);
                        Ok::<_, rquickjs::Error>(())
                    })
                    .unwrap();
            }
        }
    }

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

    #[test]
    fn cached_global_bytecode_reinitializes_in_every_fresh_runtime() {
        let artifact = SkillArtifact::new(
            "let calls = 0; function next_value() { return ++calls; }".to_string(),
            "bytecode cache fixture".to_string(),
            Vec::new(),
            vec![SkillExport {
                name: "next_value".to_string(),
                signature: "next_value()".to_string(),
            }],
            vec!["next_value() === 1".to_string()],
            CapabilityManifest::pure(),
        )
        .unwrap();
        let bytecode = compile_artifact_bytecode(&artifact).unwrap();
        assert!(!bytecode.is_empty());

        for _ in 0..2 {
            let runtime = Runtime::new().unwrap();
            let context = Context::full(&runtime).unwrap();
            context
                .with(|ctx| {
                    evaluate_global_bytecode(&ctx, bytecode.for_artifact(&artifact).unwrap())?;
                    assert_eq!(ctx.eval::<i32, _>("next_value()")?, 1);
                    Ok::<_, rquickjs::Error>(())
                })
                .unwrap();
        }
    }

    #[test]
    fn cached_compiler_preserves_global_script_grammar() {
        let artifact = SkillArtifact::new(
            "return 1; function unreachable() { return 0; }".to_string(),
            "invalid global script fixture".to_string(),
            Vec::new(),
            vec![SkillExport {
                name: "unreachable".to_string(),
                signature: "unreachable()".to_string(),
            }],
            vec!["true".to_string()],
            CapabilityManifest::pure(),
        )
        .unwrap();

        assert!(matches!(
            compile_artifact_bytecode(&artifact),
            Err(RealmError::Initialization)
        ));
    }
}
