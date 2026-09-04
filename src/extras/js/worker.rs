//! Synchronous bootstrap and fresh-runtime execution for the brokered JavaScript worker.
//!
//! Every request owns its QuickJS [`Runtime`] and [`Context`]. Neither is stored in worker state,
//! and every JavaScript value is converted to a bounded, closed Rust protocol value before the
//! terminal frame is written. The only global installed here is a bounded `console`; authority
//! globals and module loaders are deliberately absent.

use std::io::Write;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(feature = "sandbox")]
use rquickjs::prelude::Opt;
use rquickjs::promise::PromiseState;
use rquickjs::{
    Context, Ctx, Error, Function, IntoJs, Module, Object, Persistent, Runtime, Value, WriteOptions,
};

use super::protocol::{
    AdvisoryAttribution, BuildIdentity, ConsoleLevel, ConsoleRecord, Diagnostic, DiagnosticClass,
    DiagnosticStage, EffectErrorCode, EffectOperation, EffectRequest, EffectResponse, EffectResult,
    JsErrorCode, ParentFrame, ParentWireFrame, ProtocolError, ProtocolFault, ProtocolFaultCode,
    ProtocolStage, RunStep, ScriptRole, StepOutcome, StepResult, VerificationCaseResult,
    VerificationResult, VerifyArtifact, WireFrame, WorkerFrame, WorkerProtocol, WorkerWireFrame,
    read_frame, write_frame,
};
#[cfg(feature = "sandbox")]
use super::protocol::{HttpHeader, HttpMethod};
#[cfg(feature = "skills")]
use super::protocol::{
    MAX_SKILL_ARTIFACTS_PER_STEP, MAX_SKILL_CALLS_PER_STEP, MAX_SKILL_CAPABILITY_GRANTS_PER_STEP,
    MAX_SKILL_EXPORTS_PER_ARTIFACT, SkillCallRequest, SkillCallResponse, SkillInvocationGrant,
};
use super::types::{
    MEMORY_LIMIT, READ_FILE_MAX_BYTES, STACK_LIMIT, STEP_TIMEOUT, WRITE_FILE_MAX_BYTES,
};
#[cfg(feature = "skills")]
use crate::extras::js::skills::capability::{InvocationAuthorization, InvocationCapabilityRuntime};
#[cfg(feature = "skills")]
use crate::extras::js::skills::telemetry::{SkillEvent, SkillEventKind, stable_invocation_id};
use crate::sandbox::worker::{
    INTERNAL_WORKER_MARKER, INTERNAL_WORKER_MARKER_VALUE, finalize_internal_worker,
    is_internal_worker_marker_present, standard_streams_are_protocol_pipes,
};

const EXIT_FAILURE: i32 = 1;
const MAX_PENDING_JOBS: usize = 10_000;
const MAX_RESULT_BYTES: usize = 64 * 1024;
const MAX_CONSOLE_RECORDS: usize = 256;
const MAX_CONSOLE_BYTES: usize = 256 * 1024;
const MAX_CONSOLE_RECORD_BYTES: usize = 8 * 1024;
const MAX_VERIFICATION_CASES: usize = 4_096;
const MAX_VERIFICATION_CASE_ID_BYTES: usize = 128;
const VERIFICATION_LOADER_VERSION: u16 = 1;
const EFFECT_PATH_MAX_BYTES: usize = READ_FILE_MAX_BYTES;
const SPAWN_ARGUMENT_MAX_COUNT: usize = 4_096;
const SPAWN_ARGUMENTS_MAX_BYTES: usize = 1024 * 1024;
#[cfg(feature = "sandbox")]
const FETCH_URL_MAX_BYTES: usize = 64 * 1024;
#[cfg(feature = "sandbox")]
const FETCH_REQUEST_HEADER_MAX_COUNT: usize = 64;
#[cfg(feature = "sandbox")]
const FETCH_REQUEST_HEADER_MAX_BYTES: usize = 16 * 1024;
#[cfg(feature = "sandbox")]
const FETCH_REQUEST_BODY_MAX_BYTES: usize = 256 * 1024;

type ModelEffectDispatcher = Rc<dyn Fn(EffectOperation) -> EffectResult>;
type WorkerEffectDispatcher = Arc<
    dyn Fn(super::protocol::GrantId, AdvisoryAttribution, EffectOperation) -> EffectResult
        + Send
        + Sync,
>;
#[cfg(feature = "skills")]
type WorkerSkillCallAuthorizer =
    Arc<dyn Fn(String, String, u32) -> Result<SkillInvocationGrant, ()> + Send + Sync>;

#[cfg(feature = "skills")]
#[derive(Clone)]
struct WorkerEventMetadata {
    skill_id: String,
    export_name: String,
    turn_id: String,
    tool_call_id: String,
}

#[cfg(feature = "skills")]
#[derive(Default)]
struct WorkerEventState {
    events: Vec<SkillEvent>,
    pending: std::collections::HashMap<String, (WorkerEventMetadata, Instant)>,
}

#[cfg(feature = "skills")]
impl WorkerEventState {
    fn injected(&mut self, skill_id: String, turn_id: String, tool_call_id: String) {
        self.events.push(worker_event(
            skill_id,
            turn_id,
            tool_call_id,
            None,
            None,
            SkillEventKind::Injected,
            None,
            None,
            None,
        ));
    }

    fn start(&mut self, id: String, metadata: WorkerEventMetadata, shape: String) {
        self.pending
            .insert(id.clone(), (metadata.clone(), Instant::now()));
        self.events.push(worker_event(
            metadata.skill_id,
            metadata.turn_id,
            metadata.tool_call_id,
            Some(id),
            Some(metadata.export_name),
            SkillEventKind::Invoked,
            None,
            None,
            Some(shape),
        ));
    }

    fn terminal(&mut self, id: &str, success: bool) {
        let Some((metadata, started)) = self.pending.remove(id) else {
            return;
        };
        self.events.push(worker_event(
            metadata.skill_id,
            metadata.turn_id,
            metadata.tool_call_id,
            Some(id.to_string()),
            Some(metadata.export_name),
            if success {
                SkillEventKind::Returned
            } else {
                SkillEventKind::Threw
            },
            Some(if success { "fulfilled" } else { "exception" }.into()),
            Some(started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64),
            None,
        ));
    }

    fn finalize_pending(&mut self, outcome: &StepOutcome) {
        let pending = self.pending.keys().cloned().collect::<Vec<_>>();
        for id in pending {
            let Some((metadata, started)) = self.pending.remove(&id) else {
                continue;
            };
            let (kind, code) = match outcome {
                StepOutcome::Timeout => (SkillEventKind::TimedOut, "step_timeout"),
                StepOutcome::OutOfMemory => (SkillEventKind::Oom, "step_oom"),
                _ => (SkillEventKind::Threw, "step_failed"),
            };
            self.events.push(worker_event(
                metadata.skill_id,
                metadata.turn_id,
                metadata.tool_call_id,
                Some(id),
                Some(metadata.export_name),
                kind,
                Some(code.into()),
                Some(started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64),
                None,
            ));
        }
    }
}

#[cfg(feature = "skills")]
#[allow(clippy::too_many_arguments)]
fn worker_event(
    skill_id: String,
    turn_id: String,
    tool_call_id: String,
    invocation_id: Option<String>,
    export_name: Option<String>,
    kind: SkillEventKind,
    outcome: Option<String>,
    latency_us: Option<u64>,
    argument_shape: Option<String>,
) -> SkillEvent {
    SkillEvent {
        invocation_id,
        skill_id,
        turn_id,
        tool_call_id: Some(tool_call_id),
        kind,
        export_name,
        outcome,
        latency_us,
        retrieval_score: None,
        retrieval_rank: None,
        query_fingerprint: None,
        index_generation: 0,
        evidence_complete: false,
        production: false,
        argument_shape,
        created_at: 0,
    }
}

struct WorkerSpawnResult {
    stdout: String,
    stderr: String,
    code: i32,
    timed_out: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl<'js> IntoJs<'js> for WorkerSpawnResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set("stdout", self.stdout)?;
        object.set("stderr", self.stderr)?;
        object.set("code", self.code)?;
        object.set("timed_out", self.timed_out)?;
        object.set("stdout_truncated", self.stdout_truncated)?;
        object.set("stderr_truncated", self.stderr_truncated)?;
        Ok(object.into())
    }
}

#[cfg(feature = "sandbox")]
struct WorkerFetchResult {
    status: u16,
    text: String,
}

#[cfg(feature = "sandbox")]
impl<'js> IntoJs<'js> for WorkerFetchResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set("status", self.status)?;
        object.set("text", self.text)?;
        Ok(object.into())
    }
}

fn install_model_effect_globals(
    context: &Context,
    effects: ModelEffectDispatcher,
) -> rquickjs::Result<()> {
    context.with(|ctx| {
        let read_effects = effects.clone();
        let read_file = Function::new(ctx.clone(), move |path: String| {
            validate_path(&path).map_err(|code| effect_error("read_file", code))?;
            match read_effects(EffectOperation::ReadFile { path }) {
                EffectResult::ReadFile { content } => Ok(content),
                EffectResult::Error(error) => Err(effect_error("read_file", error.code)),
                _ => Err(rquickjs::Error::Unknown),
            }
        })?;
        let write_effects = effects.clone();
        let write_file = Function::new(ctx.clone(), move |path: String, content: String| {
            validate_path(&path).map_err(|code| effect_error("write_file", code))?;
            if content.len() > WRITE_FILE_MAX_BYTES {
                return Err(effect_error("write_file", EffectErrorCode::OutputLimit));
            }
            match write_effects(EffectOperation::WriteFile { path, content }) {
                EffectResult::WriteFile => Ok(()),
                EffectResult::Error(error) => Err(effect_error("write_file", error.code)),
                _ => Err(rquickjs::Error::Unknown),
            }
        })?;
        let spawn_effects = effects.clone();
        let spawn = Function::new(
            ctx.clone(),
            move |program: String, arguments: Vec<String>| {
                validate_spawn(&program, &arguments).map_err(|code| effect_error("spawn", code))?;
                match spawn_effects(EffectOperation::Spawn { program, arguments }) {
                    EffectResult::Spawn {
                        stdout,
                        stderr,
                        exit_code,
                        timed_out,
                        stdout_truncated,
                        stderr_truncated,
                    } => Ok(WorkerSpawnResult {
                        stdout,
                        stderr,
                        code: exit_code,
                        timed_out,
                        stdout_truncated,
                        stderr_truncated,
                    }),
                    EffectResult::Error(error) => Err(effect_error("spawn", error.code)),
                    _ => Err(rquickjs::Error::Unknown),
                }
            },
        )?;
        #[cfg(feature = "sandbox")]
        let fetch = {
            let fetch_effects = effects;
            Function::new(ctx.clone(), move |url: String, options: Opt<Object<'_>>| {
                if url.is_empty() || url.contains('\0') || url.len() > FETCH_URL_MAX_BYTES {
                    return Err(effect_error("fetch", EffectErrorCode::InvalidTarget));
                }
                let (method, headers, body) = parse_fetch_options(options.0.as_ref())?;
                match fetch_effects(EffectOperation::Fetch {
                    url,
                    method,
                    headers,
                    body,
                }) {
                    EffectResult::Fetch { status, body, .. } => {
                        Ok(WorkerFetchResult { status, text: body })
                    }
                    EffectResult::Error(error) => Err(effect_error("fetch", error.code)),
                    _ => Err(rquickjs::Error::Unknown),
                }
            })?
        };
        ctx.globals().set("read_file", read_file)?;
        ctx.globals().set("write_file", write_file)?;
        ctx.globals().set("spawn", spawn)?;
        #[cfg(feature = "sandbox")]
        ctx.globals().set("fetch", fetch)?;
        Ok(())
    })
}

#[cfg(feature = "skills")]
fn install_proposal_global(
    context: &Context,
    effects: ModelEffectDispatcher,
) -> rquickjs::Result<()> {
    context.with(|ctx| {
        let propose_skill = Function::new(ctx.clone(), move |draft: Object<'_>| {
            let proposal = super::skills::proposal::JsProposal::from_object(&draft)
                .map_err(|_| effect_error("propose_skill", EffectErrorCode::InvalidTarget))?;
            match effects(EffectOperation::ProposeSkill {
                draft: proposal.into(),
            }) {
                EffectResult::ProposalAccepted {
                    skill_id,
                    proposal_id,
                    status,
                    report_id,
                } => serde_json::to_string(&serde_json::json!({
                    "id": skill_id,
                    "proposal_id": proposal_id,
                    "status": status.as_str(),
                    "report_id": report_id,
                }))
                .map_err(|_| effect_error("propose_skill", EffectErrorCode::BackendFailure)),
                EffectResult::Error(error) => Err(effect_error("propose_skill", error.code)),
                _ => Err(rquickjs::Error::Unknown),
            }
        })?;
        ctx.globals().set("propose_skill", propose_skill)
    })
}

fn validate_path(path: &str) -> Result<(), EffectErrorCode> {
    if path.is_empty() || path.contains('\0') || path.len() > EFFECT_PATH_MAX_BYTES {
        Err(EffectErrorCode::InvalidTarget)
    } else {
        Ok(())
    }
}

fn validate_spawn(program: &str, arguments: &[String]) -> Result<(), EffectErrorCode> {
    if program.is_empty()
        || program.contains('\0')
        || arguments.len() > SPAWN_ARGUMENT_MAX_COUNT
        || arguments.iter().any(|argument| argument.contains('\0'))
    {
        return Err(EffectErrorCode::InvalidTarget);
    }
    let total_bytes = arguments.iter().try_fold(program.len(), |total, argument| {
        total.checked_add(argument.len())
    });
    if total_bytes.is_none_or(|total| total > SPAWN_ARGUMENTS_MAX_BYTES) {
        Err(EffectErrorCode::OutputLimit)
    } else {
        Ok(())
    }
}

#[cfg(feature = "sandbox")]
fn parse_fetch_options(
    options: Option<&Object<'_>>,
) -> rquickjs::Result<(HttpMethod, Vec<HttpHeader>, Option<String>)> {
    let Some(options) = options else {
        return Ok((HttpMethod::Get, Vec::new(), None));
    };
    for key in options.keys::<String>() {
        let key = key?;
        if !matches!(key.as_str(), "method" | "headers" | "body") {
            return Err(rquickjs::Error::new_from_js_message(
                "fetch options",
                "fetch",
                format!("unsupported field '{key}'"),
            ));
        }
    }
    let method = options
        .get::<_, Option<String>>("method")?
        .unwrap_or_else(|| "GET".into())
        .to_ascii_uppercase();
    let method = match method.as_str() {
        "GET" => HttpMethod::Get,
        "POST" => HttpMethod::Post,
        _ => {
            return Err(rquickjs::Error::new_from_js_message(
                "fetch options",
                "fetch",
                "method must be GET or POST",
            ));
        }
    };
    let mut headers = Vec::new();
    let mut header_bytes = 0_usize;
    if let Some(object) = options.get::<_, Option<Object<'_>>>("headers")? {
        for property in object.props::<String, String>() {
            let (name, value) = property?;
            if headers.len() == FETCH_REQUEST_HEADER_MAX_COUNT {
                return Err(fetch_options_error(
                    "request headers exceed the configured limit",
                ));
            }
            reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| fetch_options_error("invalid header name"))?;
            reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| fetch_options_error("invalid header value"))?;
            header_bytes = header_bytes
                .checked_add(name.len())
                .and_then(|total| total.checked_add(value.len()))
                .ok_or_else(|| {
                    fetch_options_error("request headers exceed the configured limit")
                })?;
            if header_bytes > FETCH_REQUEST_HEADER_MAX_BYTES {
                return Err(fetch_options_error(
                    "request headers exceed the configured limit",
                ));
            }
            let lower = name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "proxy-connection"
                    | "upgrade"
                    | "te"
                    | "proxy-authorization"
                    | "authorization"
                    | "cookie"
                    | "forwarded"
                    | "x-forwarded-for"
                    | "x-forwarded-host"
                    | "x-forwarded-proto"
                    | "x-real-ip"
                    | "via"
            ) {
                return Err(fetch_options_error(format!(
                    "header '{lower}' is controlled by the host"
                )));
            }
            headers.push(HttpHeader { name, value });
        }
    }
    let body = options.get::<_, Option<String>>("body")?;
    if body
        .as_ref()
        .is_some_and(|body| body.len() > FETCH_REQUEST_BODY_MAX_BYTES)
    {
        return Err(rquickjs::Error::new_from_js_message(
            "fetch options",
            "fetch",
            "request body exceeds the configured limit",
        ));
    }
    if method == HttpMethod::Get && body.is_some() {
        return Err(rquickjs::Error::new_from_js_message(
            "fetch options",
            "fetch",
            "GET requests cannot have a body",
        ));
    }
    Ok((method, headers, body))
}

#[cfg(feature = "sandbox")]
fn fetch_options_error(message: impl Into<String>) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("fetch options", "fetch", message.into())
}

fn effect_error(tool: &'static str, code: EffectErrorCode) -> rquickjs::Error {
    let code = match code {
        EffectErrorCode::Denied => "denied",
        EffectErrorCode::CapabilityDenied => "capability_denied",
        EffectErrorCode::InvalidTarget => "invalid_target",
        EffectErrorCode::Cancelled => "cancelled",
        EffectErrorCode::TimedOut => "timed_out",
        EffectErrorCode::OutputLimit => "output_limit",
        EffectErrorCode::BackendFailure => "backend_failure",
        EffectErrorCode::AuditFailure => "audit_failure",
        EffectErrorCode::OutcomeUnknown => "outcome_unknown",
    };
    rquickjs::Error::new_from_js_message("parent effect", tool, code)
}

/// Worker-owned revocation boundary for all invocation capabilities tied to one fresh runtime.
/// Dropping it covers timeout, protocol cancellation, panic unwinding, and worker recycle paths.
#[cfg(feature = "skills")]
pub(crate) struct WorkerCapabilityLifecycle {
    capabilities: InvocationCapabilityRuntime,
}

#[cfg(feature = "skills")]
impl WorkerCapabilityLifecycle {
    pub(crate) fn new(capabilities: InvocationCapabilityRuntime) -> Self {
        Self { capabilities }
    }

    pub(crate) fn cancel(&self, invocation_id: &super::protocol::InvocationId) {
        self.capabilities.cancel(invocation_id);
    }
}

#[cfg(feature = "skills")]
impl Drop for WorkerCapabilityLifecycle {
    fn drop(&mut self) {
        self.capabilities.recycle();
    }
}

const CONSOLE_WRAPPER_SOURCE: &str = r#"
(emit => {
    const string = String;
    const uncurryThis = Function.prototype.bind.bind(Function.prototype.call);
    const slice = uncurryThis(String.prototype.slice);
    const charCodeAt = uncurryThis(String.prototype.charCodeAt);
    const maximum = 8192;
    function take(value, budget) {
        let bytes = 0;
        let end = 0;
        for (let index = 0; index < value.length; index += 1) {
            const unit = charCodeAt(value, index);
            let cost;
            if (unit <= 0x7f) cost = 1;
            else if (unit <= 0x7ff) cost = 2;
            else if (unit >= 0xd800 && unit <= 0xdbff && index + 1 < value.length) {
                const next = charCodeAt(value, index + 1);
                if (next >= 0xdc00 && next <= 0xdfff) { cost = 4; index += 1; }
                else cost = 3;
            } else cost = 3;
            if (bytes + cost > budget) break;
            bytes += cost;
            end = index + 1;
        }
        return slice(value, 0, end);
    }
    function byteLength(value) {
        let bytes = 0;
        for (let index = 0; index < value.length; index += 1) {
            const unit = charCodeAt(value, index);
            if (unit <= 0x7f) bytes += 1;
            else if (unit <= 0x7ff) bytes += 2;
            else if (unit >= 0xd800 && unit <= 0xdbff && index + 1 < value.length) {
                const next = charCodeAt(value, index + 1);
                if (next >= 0xdc00 && next <= 0xdfff) { bytes += 4; index += 1; }
                else bytes += 3;
            } else bytes += 3;
        }
        return bytes;
    }
    return (...values) => {
        let text = "";
        let remaining = maximum;
        let truncated = false;
        for (let index = 0; index < values.length; index += 1) {
            const part = string(values[index]);
            if (index !== 0) {
                if (remaining === 0) { truncated = true; break; }
                text += " ";
                remaining -= 1;
            }
            const bounded = take(part, remaining);
            text += bounded;
            remaining -= byteLength(bounded);
            if (bounded.length !== part.length) {
                truncated = true;
                break;
            }
        }
        emit(text, truncated);
    };
})
"#;

const STRING_GATE_SOURCE: &str = r#"
(() => {
    const uncurryThis = Function.prototype.bind.bind(Function.prototype.call);
    const charCodeAt = uncurryThis(String.prototype.charCodeAt);
    return value => {
        let bytes = 0;
        for (let index = 0; index < value.length; index += 1) {
            const unit = charCodeAt(value, index);
            if (unit <= 0x7f) bytes += 1;
            else if (unit <= 0x7ff) bytes += 2;
            else if (unit >= 0xd800 && unit <= 0xdbff && index + 1 < value.length) {
                const next = charCodeAt(value, index + 1);
                if (next >= 0xdc00 && next <= 0xdfff) { bytes += 4; index += 1; }
                else bytes += 3;
            } else bytes += 3;
            if (bytes > 65536) throw 0;
        }
        return value;
    };
})()
"#;

pub(super) const STRICT_CLONE_SOURCE: &str = r#"
(() => {
    const uncurryThis = Function.prototype.bind.bind(Function.prototype.call);
    const getPrototypeOf = Object.getPrototypeOf;
    const setPrototypeOf = Object.setPrototypeOf;
    const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
    const create = Object.create;
    const defineProperty = Object.defineProperty;
    const ownKeys = Reflect.ownKeys;
    const isArray = Array.isArray;
    const stringify = JSON.stringify;
    const objectPrototype = Object.prototype;
    const arrayPrototype = Array.prototype;
    const finite = Number.isFinite;
    const safeInteger = Number.isSafeInteger;
    const integer = Number.isInteger;
    const number = Number;
    const string = String;
    const charCodeAt = uncurryThis(String.prototype.charCodeAt);
    const SetCtor = Set;
    const setAdd = uncurryThis(Set.prototype.add);
    const setHas = uncurryThis(Set.prototype.has);
    const setDelete = uncurryThis(Set.prototype.delete);
    const maxDepth = 64;
    const maxNodes = 10000;
    const maxBytes = 65536;

    function utf8Bytes(text) {
        let bytes = 0;
        for (let index = 0; index < text.length; index += 1) {
            const unit = charCodeAt(text, index);
            if (unit <= 0x7f) bytes += 1;
            else if (unit <= 0x7ff) bytes += 2;
            else if (unit >= 0xd800 && unit <= 0xdbff && index + 1 < text.length) {
                const next = charCodeAt(text, index + 1);
                if (next >= 0xdc00 && next <= 0xdfff) { bytes += 4; index += 1; }
                else bytes += 3;
            } else bytes += 3;
            if (bytes > maxBytes) throw 0;
        }
        return bytes;
    }

    return function strictClone(candidate) {
        // Reflect.ownKeys creates an engine Array before its length can be budgeted. Reject
        // numeric Array-prototype pollution up to and including the accepted key budget first,
        // so construction cannot dispatch an attacker setter along any accepted path.
        for (let index = 0; index <= maxNodes; index += 1) {
            if (getOwnPropertyDescriptor(arrayPrototype, string(index)) !== undefined) throw 0;
        }
        let nodes = 0;
        let bytes = 0;
        const active = new SetCtor();

        function clone(value, depth) {
            if (depth > maxDepth || ++nodes > maxNodes) throw 0;
            if (value === null || typeof value === "boolean") return value;
            if (typeof value === "number") {
                if (!finite(value)) throw 0;
                return value;
            }
            if (typeof value === "string") {
                bytes += utf8Bytes(value);
                if (bytes > maxBytes) throw 0;
                return value;
            }
            if (typeof value !== "object" || setHas(active, value)) throw 0;

            setAdd(active, value);
            const keys = ownKeys(value);
            if (keys.length > maxNodes - nodes) throw 0;
            let copy;
            if (isArray(value)) {
                const length = value.length;
                if (!safeInteger(length) || length < 0 || length > maxNodes - nodes) throw 0;
                if (keys.length !== length + 1) throw 0;
                copy = [];
                setPrototypeOf(copy, null);
                for (let index = 0; index < length; index += 1) {
                    const key = string(index);
                    const descriptor = getOwnPropertyDescriptor(value, key);
                    if (!descriptor || !descriptor.enumerable || !("value" in descriptor)) throw 0;
                    defineProperty(copy, key, {
                        value: clone(descriptor.value, depth + 1),
                        enumerable: true,
                        configurable: true,
                        writable: true,
                    });
                }
                const lengthDescriptor = getOwnPropertyDescriptor(value, "length");
                if (!lengthDescriptor || !("value" in lengthDescriptor)) throw 0;
                for (let keyIndex = 0; keyIndex < keys.length; keyIndex += 1) {
                    const key = keys[keyIndex];
                    if (key === "length") continue;
                    if (typeof key !== "string") throw 0;
                    const index = number(key);
                    if (!integer(index) || index < 0 || index >= length || string(index) !== key) throw 0;
                }
            } else {
                const prototype = getPrototypeOf(value);
                if (prototype !== objectPrototype && prototype !== null) throw 0;
                copy = create(null);
                for (let keyIndex = 0; keyIndex < keys.length; keyIndex += 1) {
                    const key = keys[keyIndex];
                    if (typeof key !== "string") throw 0;
                    bytes += utf8Bytes(key);
                    if (bytes > maxBytes) throw 0;
                    const descriptor = getOwnPropertyDescriptor(value, key);
                    if (!descriptor || !descriptor.enumerable || !("value" in descriptor)) throw 0;
                    defineProperty(copy, key, {
                        value: clone(descriptor.value, depth + 1),
                        enumerable: true,
                        configurable: true,
                        writable: true,
                    });
                }
            }
            setDelete(active, value);
            return copy;
        }

        const encoded = stringify(clone(candidate, 0));
        if (utf8Bytes(encoded) > maxBytes) throw 0;
        return encoded;
    };
})()
"#;

const TRUSTED_BOOTSTRAP_MODULE_NAME: &str = "mini-agent:trusted-bootstrap";
static TRUSTED_BOOTSTRAP_BYTECODE: OnceLock<Option<Vec<u8>>> = OnceLock::new();

fn trusted_bootstrap_source() -> String {
    format!(
        "export const strictClone = {STRICT_CLONE_SOURCE};\n\
         export const stringGate = {STRING_GATE_SOURCE};"
    )
}

fn compile_trusted_bootstrap_bytecode() -> rquickjs::Result<Vec<u8>> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + STEP_TIMEOUT;
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    let context = Context::full(&runtime)?;
    context.with(|ctx| {
        Module::declare(
            ctx,
            TRUSTED_BOOTSTRAP_MODULE_NAME,
            trusted_bootstrap_source(),
        )?
        .write(WriteOptions::default())
    })
}

fn trusted_bootstrap_bytecode() -> Option<&'static [u8]> {
    TRUSTED_BOOTSTRAP_BYTECODE
        .get_or_init(|| compile_trusted_bootstrap_bytecode().ok())
        .as_deref()
}

#[allow(unsafe_code)]
fn load_trusted_bootstrap_functions(
    context: &Context,
    bytecode: &[u8],
) -> rquickjs::Result<(Persistent<Function<'static>>, Persistent<Function<'static>>)> {
    context.with(|ctx| {
        // SAFETY: these bytes are compiled once in this process from the two
        // trusted constants above, with the same linked QuickJS ABI, and are
        // never accepted from disk, IPC, model output, or any other input.
        let module = unsafe { Module::load(ctx.clone(), bytecode)? };
        let (module, evaluation) = module.eval()?;
        evaluation.finish::<()>()?;
        let clone = module.get::<_, Function>("strictClone")?;
        let string_gate = module.get::<_, Function>("stringGate")?;
        Ok((
            Persistent::save(&ctx, clone),
            Persistent::save(&ctx, string_gate),
        ))
    })
}

#[cfg(test)]
mod trusted_bootstrap_bytecode_tests {
    use super::*;

    const BENCHMARK_WARMUPS: usize = 5;
    const BENCHMARK_SAMPLES: usize = 50;

    fn configure_benchmark_runtime() -> rquickjs::Result<(Runtime, Context)> {
        let runtime = Runtime::new()?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        runtime.set_max_stack_size(STACK_LIMIT);
        let deadline = Instant::now() + STEP_TIMEOUT;
        runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
        let context = Context::full(&runtime)?;
        Ok((runtime, context))
    }

    fn evaluate_trusted_bootstrap_source() -> rquickjs::Result<()> {
        let (_runtime, context) = configure_benchmark_runtime()?;
        context.with(|ctx| {
            let (module, evaluation) = Module::declare(
                ctx,
                TRUSTED_BOOTSTRAP_MODULE_NAME,
                trusted_bootstrap_source(),
            )?
            .eval()?;
            evaluation.finish::<()>()?;
            let _: Function = module.get("strictClone")?;
            let _: Function = module.get("stringGate")?;
            Ok(())
        })
    }

    fn load_trusted_bootstrap_bytecode_for_benchmark(bytecode: &[u8]) -> rquickjs::Result<()> {
        let (_runtime, context) = configure_benchmark_runtime()?;
        let _ = load_trusted_bootstrap_functions(&context, bytecode)?;
        Ok(())
    }

    fn percentile_microseconds(samples: &[Duration], percentile: usize) -> f64 {
        assert!(!samples.is_empty());
        assert!((1..=100).contains(&percentile));
        let mut ordered = samples.to_vec();
        ordered.sort_unstable();
        let rank = (ordered.len() * percentile).div_ceil(100);
        ordered[rank.saturating_sub(1)].as_secs_f64() * 1_000_000.0
    }

    #[test]
    fn trusted_bootstrap_bytecode_loads_into_distinct_fresh_runtimes() {
        let bytecode = compile_trusted_bootstrap_bytecode().expect("compile trusted bootstrap");

        for expected in ["{\"runtime\":1}", "{\"runtime\":2}"] {
            let runtime = Runtime::new().expect("create fresh runtime");
            runtime.set_memory_limit(MEMORY_LIMIT);
            runtime.set_max_stack_size(STACK_LIMIT);
            let deadline = Instant::now() + STEP_TIMEOUT;
            runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
            let context = Context::full(&runtime).expect("create fresh context");
            let (clone, string_gate) =
                load_trusted_bootstrap_functions(&context, &bytecode).expect("load bytecode");

            context.with(|ctx| {
                let clone = clone.clone().restore(&ctx).expect("restore strict clone");
                let string_gate = string_gate
                    .clone()
                    .restore(&ctx)
                    .expect("restore string gate");
                let value: Object = ctx.eval(format!("({expected})")).expect("create value");
                let encoded: String = clone.call((value,)).expect("clone value");
                assert_eq!(encoded, expected);
                let gated: String = string_gate.call((expected,)).expect("gate string");
                assert_eq!(gated, expected);
            });
        }
    }

    #[test]
    fn bootstrap_benchmark_percentiles_use_nearest_rank() {
        let samples = [
            Duration::from_micros(1),
            Duration::from_micros(2),
            Duration::from_micros(3),
            Duration::from_micros(4),
            Duration::from_micros(100),
        ];
        assert_eq!(percentile_microseconds(&samples, 50), 3.0);
        assert_eq!(percentile_microseconds(&samples, 95), 100.0);
    }

    #[test]
    #[ignore = "run explicitly for bounded trusted-bootstrap before/after measurements"]
    fn trusted_bootstrap_latency_benchmark() {
        assert_eq!(
            std::env::var("MINI_AGENT_JS_BOOTSTRAP_BENCH").as_deref(),
            Ok("1"),
            "set MINI_AGENT_JS_BOOTSTRAP_BENCH=1 for an intentional benchmark run"
        );
        let bytecode = compile_trusted_bootstrap_bytecode().expect("compile trusted bootstrap");
        let mut source_samples = Vec::with_capacity(BENCHMARK_SAMPLES);
        let mut bytecode_samples = Vec::with_capacity(BENCHMARK_SAMPLES);

        println!(
            "trusted bootstrap benchmark: {} warmups + {} samples per path",
            BENCHMARK_WARMUPS, BENCHMARK_SAMPLES
        );
        for iteration in 0..(BENCHMARK_WARMUPS + BENCHMARK_SAMPLES) {
            let (source_elapsed, bytecode_elapsed) = if iteration % 2 == 0 {
                let source_started = Instant::now();
                evaluate_trusted_bootstrap_source().expect("evaluate trusted bootstrap source");
                let source_elapsed = source_started.elapsed();

                let bytecode_started = Instant::now();
                load_trusted_bootstrap_bytecode_for_benchmark(&bytecode)
                    .expect("load trusted bootstrap bytecode");
                (source_elapsed, bytecode_started.elapsed())
            } else {
                let bytecode_started = Instant::now();
                load_trusted_bootstrap_bytecode_for_benchmark(&bytecode)
                    .expect("load trusted bootstrap bytecode");
                let bytecode_elapsed = bytecode_started.elapsed();

                let source_started = Instant::now();
                evaluate_trusted_bootstrap_source().expect("evaluate trusted bootstrap source");
                (source_started.elapsed(), bytecode_elapsed)
            };

            if iteration >= BENCHMARK_WARMUPS {
                source_samples.push(source_elapsed);
                bytecode_samples.push(bytecode_elapsed);
            }
        }

        println!(
            "TRUSTED_BOOTSTRAP_BENCHMARK source_eval_p50_us={:.1} source_eval_p95_us={:.1} bytecode_p50_us={:.1} bytecode_p95_us={:.1}",
            percentile_microseconds(&source_samples, 50),
            percentile_microseconds(&source_samples, 95),
            percentile_microseconds(&bytecode_samples, 50),
            percentile_microseconds(&bytecode_samples, 95),
        );
    }
}

#[derive(Clone, Copy)]
struct ExecutionLimits {
    timeout: Duration,
    max_pending_jobs: usize,
}

impl ExecutionLimits {
    fn current() -> Self {
        #[cfg(test)]
        {
            let timeout = std::env::var("MINI_AGENT_TEST_WORKER_TIMEOUT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .map(Duration::from_millis)
                .unwrap_or(STEP_TIMEOUT);
            let max_pending_jobs = std::env::var("MINI_AGENT_TEST_WORKER_MAX_PENDING_JOBS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(MAX_PENDING_JOBS);
            Self {
                timeout,
                max_pending_jobs,
            }
        }
        #[cfg(not(test))]
        Self {
            timeout: STEP_TIMEOUT,
            max_pending_jobs: MAX_PENDING_JOBS,
        }
    }
}

#[derive(Clone)]
struct ClosedFailure {
    outcome: StepOutcome,
    diagnostic: Diagnostic,
}

impl ClosedFailure {
    fn error(code: JsErrorCode, stage: DiagnosticStage, role: ScriptRole) -> Self {
        let class = match code {
            JsErrorCode::Syntax => DiagnosticClass::Syntax,
            JsErrorCode::Exception => DiagnosticClass::Exception,
            JsErrorCode::StackLimit | JsErrorCode::JobLimit => DiagnosticClass::ResourceLimit,
            JsErrorCode::InvalidResult => DiagnosticClass::Contract,
            JsErrorCode::Internal => DiagnosticClass::Internal,
        };
        Self {
            outcome: StepOutcome::Error(code),
            diagnostic: diagnostic(class, stage, role),
        }
    }

    fn timeout(stage: DiagnosticStage, role: ScriptRole) -> Self {
        Self {
            outcome: StepOutcome::Timeout,
            diagnostic: diagnostic(DiagnosticClass::ResourceLimit, stage, role),
        }
    }

    fn out_of_memory(stage: DiagnosticStage, role: ScriptRole) -> Self {
        Self {
            outcome: StepOutcome::OutOfMemory,
            diagnostic: diagnostic(DiagnosticClass::ResourceLimit, stage, role),
        }
    }
}

fn diagnostic(class: DiagnosticClass, stage: DiagnosticStage, role: ScriptRole) -> Diagnostic {
    Diagnostic {
        class,
        stage,
        script_role: role,
    }
}

/// Enter internal-worker mode when and only when the reserved launcher marker is present.
pub(crate) fn maybe_run_internal_worker() -> Option<ExitCode> {
    if !is_internal_worker_marker_present() {
        return None;
    }
    #[cfg(target_os = "linux")]
    if std::env::var_os(INTERNAL_WORKER_MARKER).as_deref()
        == Some(std::ffi::OsStr::new(
            crate::sandbox::worker::LINUX_PREFLIGHT_MARKER_VALUE,
        ))
    {
        return Some(
            if standard_streams_are_protocol_pipes() && finalize_internal_worker().is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            },
        );
    }
    Some(if run_marked_worker() == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run_marked_worker() -> i32 {
    if std::env::var_os(INTERNAL_WORKER_MARKER).as_deref()
        != Some(std::ffi::OsStr::new(INTERNAL_WORKER_MARKER_VALUE))
    {
        return EXIT_FAILURE;
    }
    if !standard_streams_are_protocol_pipes() {
        return EXIT_FAILURE;
    }

    if bootstrap(std::io::stdin(), std::io::stdout()).is_ok() {
        0
    } else {
        EXIT_FAILURE
    }
}

fn bootstrap<R: std::io::Read + Send + 'static, W: Write + Send + 'static>(
    mut input: R,
    mut output: W,
) -> Result<(), ()> {
    let build = BuildIdentity::current();
    let limits = ExecutionLimits::current();
    let mut protocol = WorkerProtocol::new(build.clone());

    let hello: ParentWireFrame = read_frame(&mut input).map_err(|_| ())?;
    if !matches!(hello.message, ParentFrame::Hello(_)) {
        return Err(());
    }
    if let Err(error) = protocol.on_receive(&hello) {
        let code = match error {
            ProtocolError::VersionMismatch { .. } => ProtocolFaultCode::VersionMismatch,
            ProtocolError::BuildMismatch { .. } => ProtocolFaultCode::BuildMismatch,
            _ => return Err(()),
        };
        let fault = WireFrame {
            // Echo the parent's connection identity so it can authenticate and classify the
            // fault even though this worker belongs to an older in-place installation.
            protocol_version: hello.protocol_version,
            build_id: hello.build_id,
            invocation_id: None,
            sequence: hello.sequence.checked_add(1).ok_or(())?,
            message: WorkerFrame::ProtocolFault(ProtocolFault {
                code,
                stage: ProtocolStage::Handshake,
            }),
        };
        write_terminal(&mut output, &fault)?;
        return Err(());
    }

    finalize_internal_worker().map_err(|_| ())?;
    // Compile trusted, static helpers before advertising readiness. Request
    // runtimes load these bytes but remain fresh and request-local.
    trusted_bootstrap_bytecode().ok_or(())?;

    let ready: WorkerWireFrame = WireFrame::connection(
        build.clone(),
        1,
        WorkerFrame::Ready(protocol.ready().map_err(|_| ())?),
    );
    protocol.on_send(&ready).map_err(|_| ())?;
    write_terminal(&mut output, &ready)?;

    let transport = Arc::new(Mutex::new(WorkerTransport {
        input,
        output,
        protocol,
    }));

    loop {
        let request: ParentWireFrame = {
            let mut transport = transport.lock().map_err(|_| ())?;
            let request = read_frame(&mut transport.input).map_err(|_| ())?;
            transport.protocol.on_receive(&request).map_err(|_| ())?;
            request
        };
        let invocation_id = request.invocation_id.clone();
        let mut sequence = request.sequence.checked_add(1).ok_or(())?;
        let message = match request.message {
            ParentFrame::RunStep(step) => {
                let (result, terminal_sequence) = execute_brokered_run_step(
                    step,
                    limits,
                    transport.clone(),
                    build.clone(),
                    invocation_id.clone().ok_or(())?,
                    sequence,
                )?;
                sequence = terminal_sequence;
                WorkerFrame::StepResult(result)
            }
            ParentFrame::VerifyArtifact(request) => {
                WorkerFrame::VerificationResult(execute_verification(request, limits))
            }
            ParentFrame::ContainmentProbe(probe) => {
                #[cfg(target_os = "windows")]
                {
                    if !crate::sandbox::worker::attest_windows_containment(&probe) {
                        return Err(());
                    }
                    WorkerFrame::ContainmentAttested(
                        super::protocol::ContainmentAttestation::Passed,
                    )
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = probe;
                    if !crate::sandbox::worker::attest_macos_hosted_containment() {
                        return Err(());
                    }
                    WorkerFrame::ContainmentAttested(
                        super::protocol::ContainmentAttestation::Passed,
                    )
                }
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                {
                    let _ = probe;
                    return Err(());
                }
            }
            ParentFrame::Shutdown => return Ok(()),
            ParentFrame::Hello(_) | ParentFrame::EffectResponse(_) => return Err(()),
            #[cfg(feature = "skills")]
            ParentFrame::SkillCallResponse(_) => return Err(()),
        };
        let response = WireFrame {
            protocol_version: super::protocol::PROTOCOL_VERSION,
            build_id: build.clone(),
            invocation_id,
            sequence,
            message,
        };
        let mut transport = transport.lock().map_err(|_| ())?;
        transport.protocol.on_send(&response).map_err(|_| ())?;
        write_terminal(&mut transport.output, &response)?;
    }
}

fn write_terminal(output: &mut impl Write, frame: &WorkerWireFrame) -> Result<(), ()> {
    write_frame(output, frame).map_err(|_| ())?;
    output.flush().map_err(|_| ())
}

#[cfg(test)]
mod bootstrap_handshake_tests {
    use std::io::{Cursor, Write};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::extras::js::protocol::ParentProtocol;

    #[derive(Clone, Default)]
    struct SharedOutput(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn build_mismatch_emits_authenticated_handshake_fault_before_exit() {
        let parent_build = BuildIdentity::new("1.8.0+in-place-upgrade").unwrap();
        let parent_protocol = ParentProtocol::new(parent_build.clone());
        let hello = WireFrame::connection(
            parent_build.clone(),
            0,
            ParentFrame::Hello(parent_protocol.hello()),
        );
        let mut input = Vec::new();
        write_frame(&mut input, &hello).unwrap();
        let sink = SharedOutput::default();

        assert!(bootstrap(Cursor::new(input), sink.clone()).is_err());
        let bytes = sink.0.lock().unwrap().clone();
        let fault: WorkerWireFrame = read_frame(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(fault.protocol_version, hello.protocol_version);
        assert_eq!(fault.build_id, parent_build);
        assert_eq!(fault.sequence, 1);
        assert_eq!(fault.invocation_id, None);
        assert_eq!(
            fault.message,
            WorkerFrame::ProtocolFault(ProtocolFault {
                code: ProtocolFaultCode::BuildMismatch,
                stage: ProtocolStage::Handshake,
            })
        );
    }
}

fn execute_brokered_run_step<R: std::io::Read + Send + 'static, W: Write + Send + 'static>(
    request: RunStep,
    limits: ExecutionLimits,
    transport: Arc<Mutex<WorkerTransport<R, W>>>,
    build: BuildIdentity,
    invocation_id: super::protocol::InvocationId,
    sequence: u64,
) -> Result<(StepResult, u64), ()> {
    let model_grant_id = request.model_grant_id.clone();
    let ordinal = Arc::new(std::sync::atomic::AtomicU32::new(0));
    #[cfg(feature = "skills")]
    let skill_request_ordinal = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let sequence = Arc::new(Mutex::new(sequence));
    let protocol_failed = Arc::new(AtomicBool::new(false));
    let wire_dispatcher: WorkerEffectDispatcher = {
        let effect_build = build.clone();
        let effect_invocation_id = invocation_id.clone();
        let ordinal = ordinal.clone();
        let sequence = sequence.clone();
        let protocol_failed = protocol_failed.clone();
        let transport = transport.clone();
        Arc::new(move |grant_id, advisory, operation| {
            if protocol_failed.load(Ordering::Acquire) {
                return backend_failure();
            }
            let effect_ordinal = ordinal.fetch_add(1, Ordering::AcqRel);
            if effect_ordinal >= super::protocol::MAX_EFFECTS_PER_STEP {
                protocol_failed.store(true, Ordering::Release);
                return backend_failure();
            }
            let request = EffectRequest {
                effect_ordinal,
                grant_id,
                advisory,
                operation,
            };
            let result = transport.lock().map_err(|_| ()).and_then(|mut transport| {
                transport.round_trip(request, &effect_build, &effect_invocation_id, &sequence)
            });
            match result {
                Ok(result) => {
                    if matches!(
                        &result,
                        EffectResult::Error(super::protocol::EffectError {
                            code: EffectErrorCode::OutcomeUnknown,
                        })
                    ) {
                        protocol_failed.store(true, Ordering::Release);
                    }
                    result
                }
                Err(()) => {
                    protocol_failed.store(true, Ordering::Release);
                    backend_failure()
                }
            }
        })
    };
    let model_dispatcher = model_grant_id.map(|grant_id| {
        let dispatcher = wire_dispatcher.clone();
        Rc::new(move |operation| {
            dispatcher(grant_id.clone(), AdvisoryAttribution::default(), operation)
        }) as ModelEffectDispatcher
    });
    #[cfg(feature = "skills")]
    let skill_call_authorizer = {
        let skill_request_ordinal = skill_request_ordinal.clone();
        let sequence = sequence.clone();
        let protocol_failed = protocol_failed.clone();
        let transport = transport.clone();
        let build = build.clone();
        let invocation_id = invocation_id.clone();
        Arc::new(
            move |artifact_id: String, export_name: String, call_ordinal: u32| {
                if protocol_failed.load(Ordering::Acquire) {
                    return Err(());
                }
                let request_ordinal = skill_request_ordinal.fetch_add(1, Ordering::AcqRel);
                if request_ordinal >= MAX_SKILL_CALLS_PER_STEP {
                    protocol_failed.store(true, Ordering::Release);
                    return Err(());
                }
                let request = SkillCallRequest {
                    request_ordinal,
                    artifact_id,
                    export_name,
                    call_ordinal,
                };
                let result = transport.lock().map_err(|_| ()).and_then(|mut transport| {
                    transport.skill_call_round_trip(request, &build, &invocation_id, &sequence)
                });
                if result.is_err() {
                    protocol_failed.store(true, Ordering::Release);
                }
                result
            },
        )
            as Arc<dyn Fn(String, String, u32) -> Result<SkillInvocationGrant, ()> + Send + Sync>
    };
    let terminal = execute_run_step(
        request,
        limits,
        model_dispatcher,
        wire_dispatcher,
        #[cfg(feature = "skills")]
        skill_call_authorizer,
    );
    if protocol_failed.load(Ordering::Acquire) {
        Err(())
    } else {
        Ok((terminal, *sequence.lock().map_err(|_| ())?))
    }
}

struct WorkerTransport<R, W> {
    input: R,
    output: W,
    protocol: WorkerProtocol,
}

impl<R: std::io::Read, W: Write> WorkerTransport<R, W> {
    fn round_trip(
        &mut self,
        request: EffectRequest,
        build: &BuildIdentity,
        invocation_id: &super::protocol::InvocationId,
        sequence: &Mutex<u64>,
    ) -> Result<EffectResult, ()> {
        let frame = WireFrame::invocation(
            build.clone(),
            invocation_id.clone(),
            *sequence.lock().map_err(|_| ())?,
            WorkerFrame::EffectRequest(request.clone()),
        );
        self.protocol.on_send(&frame).map_err(|_| ())?;
        write_terminal(&mut self.output, &frame)?;
        {
            let mut sequence = sequence.lock().map_err(|_| ())?;
            *sequence = sequence.checked_add(1).ok_or(())?;
        }
        let response: ParentWireFrame = read_frame(&mut self.input).map_err(|_| ())?;
        self.protocol.on_receive(&response).map_err(|_| ())?;
        {
            let mut sequence = sequence.lock().map_err(|_| ())?;
            *sequence = sequence.checked_add(1).ok_or(())?;
        }
        match response.message {
            ParentFrame::EffectResponse(EffectResponse {
                effect_ordinal,
                result,
            }) if effect_ordinal == request.effect_ordinal => Ok(result),
            _ => Err(()),
        }
    }

    #[cfg(feature = "skills")]
    fn skill_call_round_trip(
        &mut self,
        request: SkillCallRequest,
        build: &BuildIdentity,
        invocation_id: &super::protocol::InvocationId,
        sequence: &Mutex<u64>,
    ) -> Result<SkillInvocationGrant, ()> {
        let frame = WireFrame::invocation(
            build.clone(),
            invocation_id.clone(),
            *sequence.lock().map_err(|_| ())?,
            WorkerFrame::SkillCallRequest(request.clone()),
        );
        self.protocol.on_send(&frame).map_err(|_| ())?;
        write_terminal(&mut self.output, &frame)?;
        {
            let mut sequence = sequence.lock().map_err(|_| ())?;
            *sequence = sequence.checked_add(1).ok_or(())?;
        }
        let response: ParentWireFrame = read_frame(&mut self.input).map_err(|_| ())?;
        self.protocol.on_receive(&response).map_err(|_| ())?;
        {
            let mut sequence = sequence.lock().map_err(|_| ())?;
            *sequence = sequence.checked_add(1).ok_or(())?;
        }
        match response.message {
            ParentFrame::SkillCallResponse(SkillCallResponse {
                request_ordinal,
                authorization: Some(authorization),
            }) if request_ordinal == request.request_ordinal => Ok(authorization),
            _ => Err(()),
        }
    }
}

fn backend_failure() -> EffectResult {
    EffectResult::Error(super::protocol::EffectError {
        code: EffectErrorCode::BackendFailure,
    })
}

#[cfg(feature = "skills")]
fn prepare_bound_exports(
    request: &RunStep,
    capabilities: &InvocationCapabilityRuntime,
    events: Arc<Mutex<WorkerEventState>>,
    authorize_call: WorkerSkillCallAuthorizer,
) -> Result<
    std::collections::HashMap<
        String,
        std::collections::HashMap<String, super::realm::BoundExportInvocation>,
    >,
    (),
> {
    use std::collections::{HashMap, HashSet};

    validate_skill_authority_bounds(request)?;
    if request.artifacts.is_empty() {
        return Ok(HashMap::new());
    }
    if request.turn_id.is_empty() || request.tool_call_id.is_empty() {
        return Err(());
    }
    let mut prepared = HashMap::new();
    let mut seen_artifacts = HashSet::new();
    for artifact in &request.artifacts {
        if !seen_artifacts.insert(artifact.id.clone()) {
            return Err(());
        }
        let mut exports = HashMap::new();
        for export in &artifact.exports {
            let metadata = WorkerEventMetadata {
                skill_id: artifact.id.clone(),
                export_name: export.name.clone(),
                turn_id: request.turn_id.clone(),
                tool_call_id: request.tool_call_id.clone(),
            };
            let call_authorizer = authorize_call.clone();
            let call_capabilities = capabilities.clone();
            let call_manifest = artifact.capability.clone();
            let call_artifact_id = artifact.id.clone();
            let call_export_name = export.name.clone();
            let call_turn_id = request.turn_id.clone();
            let call_tool_call_id = request.tool_call_id.clone();
            let authorize = Arc::new(move |call_ordinal: u32| {
                let issued = call_authorizer(
                    call_artifact_id.clone(),
                    call_export_name.clone(),
                    call_ordinal,
                )?;
                let expected_invocation = stable_invocation_id(
                    &call_turn_id,
                    &call_tool_call_id,
                    &call_artifact_id,
                    &call_export_name,
                    call_ordinal,
                );
                if issued.artifact_id != call_artifact_id
                    || issued.export_name != call_export_name
                    || issued.invocation_id.as_str() != expected_invocation
                {
                    return Err(());
                }
                let authorization = InvocationAuthorization::new(
                    issued.invocation_id,
                    call_artifact_id.clone(),
                    call_export_name.clone(),
                    call_manifest.clone(),
                    issued
                        .grants
                        .into_iter()
                        .map(|grant| (grant.capability, grant.grant_id)),
                )
                .map_err(|_| ())?;
                let invocation_id = expected_invocation;
                let handle = call_capabilities.prepare(authorization).map_err(|_| ())?;
                Ok((handle, invocation_id))
            });
            let start_events = events.clone();
            let start_metadata = metadata.clone();
            let on_start = Arc::new(move |id: String, shape: String| {
                let shape = if shape.len()
                    <= crate::extras::js::skills::telemetry::MAX_ARGUMENT_SHAPE_BYTES
                {
                    shape
                } else {
                    r#"{"truncated":true}"#.to_string()
                };
                start_events.lock().map_err(|_| ())?.start(
                    id.clone(),
                    start_metadata.clone(),
                    shape,
                );
                Ok(())
            });
            let terminal_events = events.clone();
            let on_terminal = Arc::new(move |invocation_id: String, success: bool| {
                terminal_events
                    .lock()
                    .map_err(|_| ())?
                    .terminal(&invocation_id, success);
                Ok(())
            });
            exports.insert(
                export.name.clone(),
                super::realm::BoundExportInvocation {
                    authorize,
                    on_start,
                    on_terminal,
                },
            );
        }
        prepared.insert(artifact.id.clone(), exports);
    }
    Ok(prepared)
}

#[cfg(feature = "skills")]
fn validate_skill_authority_bounds(request: &RunStep) -> Result<(), ()> {
    if request.artifacts.len() > MAX_SKILL_ARTIFACTS_PER_STEP {
        return Err(());
    }
    let mut expected_grants = 0_usize;
    for artifact in &request.artifacts {
        if artifact.exports.len() > MAX_SKILL_EXPORTS_PER_ARTIFACT {
            return Err(());
        }
        expected_grants = expected_grants
            .checked_add(
                artifact
                    .exports
                    .len()
                    .checked_mul(artifact.capability.grants.len())
                    .ok_or(())?,
            )
            .ok_or(())?;
        if expected_grants > MAX_SKILL_CAPABILITY_GRANTS_PER_STEP {
            return Err(());
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "skills"))]
mod skill_authority_bound_tests {
    use super::*;
    use crate::extras::js::skills::{
        CapabilityManifest, CapabilityScope, CapabilityTier, SkillArtifact, SkillExport,
    };

    fn artifact(export_count: usize, capability: CapabilityManifest) -> SkillArtifact {
        SkillArtifact::new(
            "function unused() { return 0; }".into(),
            "worker cardinality fixture".into(),
            vec![],
            (0..export_count)
                .map(|index| SkillExport {
                    name: format!("export_{index}"),
                    signature: format!("export_{index}()"),
                })
                .collect(),
            vec!["true".into()],
            capability,
        )
        .unwrap()
    }

    fn step(artifacts: Vec<SkillArtifact>) -> RunStep {
        RunStep::new("1".into()).with_skills(
            artifacts,
            "bounded-worker-turn".into(),
            "bounded-worker-call".into(),
        )
    }

    #[test]
    fn worker_rejects_artifact_export_and_total_grant_overflow_before_preparation() {
        let pure = artifact(1, CapabilityManifest::pure());
        assert!(
            validate_skill_authority_bounds(&step(vec![pure; MAX_SKILL_ARTIFACTS_PER_STEP + 1]))
                .is_err()
        );

        let too_many_exports = artifact(
            MAX_SKILL_EXPORTS_PER_ARTIFACT + 1,
            CapabilityManifest::pure(),
        );
        assert!(validate_skill_authority_bounds(&step(vec![too_many_exports])).is_err());

        let four_grants = CapabilityManifest::new(
            CapabilityTier::SideEffecting,
            vec![
                CapabilityScope::ReadFile {
                    workspace_prefixes: vec!["Cargo.toml".into()],
                },
                CapabilityScope::WriteFile {
                    workspace_prefixes: vec!["target".into()],
                },
                CapabilityScope::Fetch {
                    origins: vec!["https://example.test".into()],
                    methods: vec![crate::extras::js::skills::HttpMethod::Get],
                },
                CapabilityScope::Spawn {
                    programs: vec!["printf".into()],
                },
            ],
        )
        .unwrap();
        let grant_heavy = artifact(MAX_SKILL_EXPORTS_PER_ARTIFACT, four_grants);
        let artifact_count =
            MAX_SKILL_CAPABILITY_GRANTS_PER_STEP / (MAX_SKILL_EXPORTS_PER_ARTIFACT * 4) + 1;
        assert!(validate_skill_authority_bounds(&step(vec![grant_heavy; artifact_count])).is_err());
    }
}

fn execute_run_step(
    request: RunStep,
    limits: ExecutionLimits,
    effects: Option<ModelEffectDispatcher>,
    _wire_effects: WorkerEffectDispatcher,
    #[cfg(feature = "skills")] authorize_skill_call: WorkerSkillCallAuthorizer,
) -> StepResult {
    let console = Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "skills")]
    let event_state = Arc::new(Mutex::new(WorkerEventState::default()));
    #[cfg(feature = "skills")]
    let capability_runtime = {
        let effects = _wire_effects.clone();
        InvocationCapabilityRuntime::new(move |effect| {
            Ok(effects(
                effect.request.grant_id,
                effect.request.advisory,
                effect.request.operation,
            ))
        })
    };
    #[cfg(feature = "skills")]
    let _capability_lifecycle = WorkerCapabilityLifecycle::new(capability_runtime.clone());
    #[cfg(feature = "skills")]
    let bindings = match prepare_bound_exports(
        &request,
        &capability_runtime,
        event_state.clone(),
        authorize_skill_call,
    ) {
        Ok(bindings) => bindings,
        Err(()) => {
            return StepResult {
                outcome: StepOutcome::Error(JsErrorCode::Internal),
                console: Vec::new(),
                diagnostic: Some(Diagnostic {
                    class: DiagnosticClass::Internal,
                    stage: DiagnosticStage::Initialization,
                    script_role: ScriptRole::SkillSource,
                }),
                skill_events: Vec::new(),
                evidence_complete: false,
            };
        }
    };
    #[cfg(feature = "skills")]
    let proposal_effects = request.proposal_grant_id.clone().map(|grant_id| {
        let effects = _wire_effects;
        Rc::new(move |operation| {
            effects(grant_id.clone(), AdvisoryAttribution::default(), operation)
        }) as ModelEffectDispatcher
    });
    let execution = execute_fresh_step(
        &request.code,
        ScriptRole::Model,
        limits,
        console.clone(),
        effects,
        #[cfg(feature = "skills")]
        proposal_effects,
        #[cfg(feature = "skills")]
        &request.artifacts,
        #[cfg(feature = "skills")]
        &bindings,
        #[cfg(feature = "skills")]
        &capability_runtime,
        #[cfg(feature = "skills")]
        event_state.clone(),
        #[cfg(feature = "skills")]
        &request.turn_id,
        #[cfg(feature = "skills")]
        &request.tool_call_id,
    );
    let console = console
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    #[cfg(feature = "skills")]
    {
        let outcome = match &execution {
            Ok(outcome) => outcome,
            Err(failure) => &failure.outcome,
        };
        if let Ok(mut state) = event_state.lock() {
            state.finalize_pending(outcome);
        }
    }
    #[cfg(feature = "skills")]
    let skill_events = event_state
        .lock()
        .map(|state| state.events.clone())
        .unwrap_or_default();
    match execution {
        Ok(outcome) => StepResult {
            outcome,
            console,
            diagnostic: None,
            #[cfg(feature = "skills")]
            skill_events,
            #[cfg(feature = "skills")]
            evidence_complete: true,
        },
        Err(failure) => StepResult {
            outcome: failure.outcome,
            console,
            diagnostic: Some(failure.diagnostic),
            #[cfg(feature = "skills")]
            skill_events,
            #[cfg(feature = "skills")]
            evidence_complete: true,
        },
    }
}

// These arguments are the complete per-request security context and keeping them
// explicit makes fresh-runtime construction and capability binding auditable.
#[allow(clippy::too_many_arguments)]
fn execute_fresh_step(
    source: &str,
    role: ScriptRole,
    limits: ExecutionLimits,
    console: Arc<Mutex<Vec<ConsoleRecord>>>,
    effects: Option<ModelEffectDispatcher>,
    #[cfg(feature = "skills")] proposal_effects: Option<ModelEffectDispatcher>,
    #[cfg(feature = "skills")] artifacts: &[super::skills::SkillArtifact],
    #[cfg(feature = "skills")] bindings: &std::collections::HashMap<
        String,
        std::collections::HashMap<String, super::realm::BoundExportInvocation>,
    >,
    #[cfg(feature = "skills")] capability_runtime: &InvocationCapabilityRuntime,
    #[cfg(feature = "skills")] event_state: Arc<Mutex<WorkerEventState>>,
    #[cfg(feature = "skills")] turn_id: &str,
    #[cfg(feature = "skills")] tool_call_id: &str,
) -> Result<StepOutcome, ClosedFailure> {
    let runtime = Runtime::new().map_err(|error| initialization_failure(error, role))?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + limits.timeout;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupt_flag = interrupted.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let expired = Instant::now() >= deadline;
        if expired {
            interrupt_flag.store(true, Ordering::Relaxed);
        }
        expired
    })));

    let context = Context::full(&runtime).map_err(|error| initialization_failure(error, role))?;
    install_console(&context, console).map_err(|error| {
        classify_error(
            &context,
            error,
            deadline,
            &interrupted,
            DiagnosticStage::Initialization,
            role,
        )
    })?;
    if let Some(effects) = effects {
        install_model_effect_globals(&context, effects).map_err(|error| {
            classify_error(
                &context,
                error,
                deadline,
                &interrupted,
                DiagnosticStage::Initialization,
                role,
            )
        })?;
    }
    #[cfg(feature = "skills")]
    if let Some(proposal_effects) = proposal_effects {
        install_proposal_global(&context, proposal_effects).map_err(|error| {
            classify_error(
                &context,
                error,
                deadline,
                &interrupted,
                DiagnosticStage::Initialization,
                role,
            )
        })?;
    }
    let bytecode = trusted_bootstrap_bytecode().ok_or_else(|| {
        ClosedFailure::error(JsErrorCode::Internal, DiagnosticStage::Initialization, role)
    })?;
    let (clone, string_gate) =
        load_trusted_bootstrap_functions(&context, bytecode).map_err(|error| {
            classify_error(
                &context,
                error,
                deadline,
                &interrupted,
                DiagnosticStage::Initialization,
                role,
            )
        })?;
    #[cfg(feature = "skills")]
    let mut loaded_artifacts = Vec::with_capacity(artifacts.len());
    #[cfg(feature = "skills")]
    for artifact in artifacts {
        let artifact_bindings = bindings.get(&artifact.id).cloned().ok_or_else(|| {
            ClosedFailure::error(
                JsErrorCode::Internal,
                DiagnosticStage::Initialization,
                ScriptRole::SkillSource,
            )
        })?;
        let loaded = super::realm::load_artifact_with_bound_exports(
            &runtime,
            &context,
            artifact,
            capability_runtime.clone(),
            artifact_bindings,
        )
        .map_err(|_| {
            ClosedFailure::error(
                JsErrorCode::Internal,
                DiagnosticStage::Initialization,
                ScriptRole::SkillSource,
            )
        })?;
        loaded_artifacts.push(loaded);
        event_state
            .lock()
            .map_err(|_| {
                ClosedFailure::error(
                    JsErrorCode::Internal,
                    DiagnosticStage::Initialization,
                    ScriptRole::SkillSource,
                )
            })?
            .injected(
                artifact.id.clone(),
                turn_id.to_string(),
                tool_call_id.to_string(),
            );
    }
    let value = evaluate(&context, source, &runtime, deadline, &interrupted, role)?;
    let mut remaining_jobs = limits.max_pending_jobs;
    drain_jobs(&runtime, deadline, &interrupted, &mut remaining_jobs, role)?;
    settle_and_convert(
        &runtime,
        &context,
        value,
        clone,
        string_gate,
        deadline,
        &interrupted,
        role,
    )
}

fn install_console(
    context: &Context,
    records: Arc<Mutex<Vec<ConsoleRecord>>>,
) -> rquickjs::Result<()> {
    context.with(|ctx| {
        let console = Object::new(ctx.clone())?;
        for (name, level) in [
            ("log", ConsoleLevel::Log),
            ("warn", ConsoleLevel::Warn),
            ("error", ConsoleLevel::Error),
        ] {
            let records = records.clone();
            let emit = Function::new(ctx.clone(), move |text: String, truncated: bool| {
                record_console(&records, level, text, truncated);
            })?;
            let wrapper: Function = ctx.eval(CONSOLE_WRAPPER_SOURCE)?;
            let function: Function = wrapper.call((emit,))?;
            console.set(name, function)?;
        }
        ctx.globals().set("console", console)
    })
}

fn record_console(
    records: &Arc<Mutex<Vec<ConsoleRecord>>>,
    level: ConsoleLevel,
    text: String,
    already_truncated: bool,
) {
    let mut records = records
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if records.len() >= MAX_CONSOLE_RECORDS {
        if let Some(last) = records.last_mut() {
            last.truncated = true;
        }
        return;
    }
    let used = records
        .iter()
        .map(|record| record.text.len())
        .sum::<usize>();
    let available = MAX_CONSOLE_BYTES.saturating_sub(used);
    if available == 0 {
        if let Some(last) = records.last_mut() {
            last.truncated = true;
        }
        return;
    }
    let maximum = available.min(MAX_CONSOLE_RECORD_BYTES);
    let bounded = truncate_utf8(&text, maximum);
    records.push(ConsoleRecord {
        level,
        truncated: already_truncated || bounded.len() < text.len(),
        text: bounded,
    });
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn evaluate(
    context: &Context,
    source: &str,
    runtime: &Runtime,
    deadline: Instant,
    interrupted: &AtomicBool,
    role: ScriptRole,
) -> Result<Persistent<Value<'static>>, ClosedFailure> {
    context
        .with(|ctx| {
            ctx.eval::<Value, _>(source)
                .map(|value| Persistent::save(&ctx, value))
        })
        .map_err(|error| {
            classify_evaluation_error(
                context,
                runtime,
                error,
                deadline,
                interrupted,
                DiagnosticStage::Evaluation,
                role,
            )
        })
}

fn drain_jobs(
    runtime: &Runtime,
    deadline: Instant,
    interrupted: &AtomicBool,
    remaining_jobs: &mut usize,
    role: ScriptRole,
) -> Result<(), ClosedFailure> {
    loop {
        if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(ClosedFailure::timeout(DiagnosticStage::JobDrain, role));
        }
        if *remaining_jobs == 0 {
            return if runtime.is_job_pending() {
                Err(ClosedFailure::error(
                    JsErrorCode::JobLimit,
                    DiagnosticStage::JobDrain,
                    role,
                ))
            } else {
                Ok(())
            };
        }
        match runtime.execute_pending_job() {
            Ok(true) => *remaining_jobs -= 1,
            Ok(false) => return Ok(()),
            Err(exception) => {
                let near_heap_limit = runtime_is_near_heap_limit(runtime);
                return Err(exception.0.with(|ctx| {
                    let _ = ctx.catch();
                    if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
                        ClosedFailure::timeout(DiagnosticStage::JobDrain, role)
                    } else if near_heap_limit {
                        ClosedFailure::out_of_memory(DiagnosticStage::JobDrain, role)
                    } else {
                        ClosedFailure::error(
                            JsErrorCode::Exception,
                            DiagnosticStage::JobDrain,
                            role,
                        )
                    }
                }));
            }
        }
    }
}

// Settlement receives the already-captured runtime guards as separate values so
// no reusable QuickJS state can be hidden in a context object.
#[allow(clippy::too_many_arguments)]
fn settle_and_convert(
    runtime: &Runtime,
    context: &Context,
    value: Persistent<Value<'static>>,
    clone: Persistent<Function<'static>>,
    string_gate: Persistent<Function<'static>>,
    deadline: Instant,
    interrupted: &AtomicBool,
    role: ScriptRole,
) -> Result<StepOutcome, ClosedFailure> {
    let near_heap_limit = runtime_is_near_heap_limit(runtime);
    context.with(|ctx| {
        let mut value = value.restore(&ctx).map_err(|error| {
            classify_ctx_error(
                &ctx,
                error,
                deadline,
                interrupted,
                DiagnosticStage::ResultConversion,
                role,
            )
        })?;
        if let Some(promise) = value.as_promise() {
            value = match promise.state() {
                PromiseState::Resolved => promise
                    .result::<Value>()
                    .and_then(Result::ok)
                    .ok_or_else(|| {
                        ClosedFailure::error(
                            JsErrorCode::Internal,
                            DiagnosticStage::ResultConversion,
                            role,
                        )
                    })?,
                PromiseState::Rejected => {
                    let _ = promise.result::<Value>();
                    let _ = ctx.catch();
                    if near_heap_limit {
                        return Err(ClosedFailure::out_of_memory(
                            DiagnosticStage::Evaluation,
                            role,
                        ));
                    }
                    return Err(ClosedFailure::error(
                        JsErrorCode::Exception,
                        DiagnosticStage::Evaluation,
                        role,
                    ));
                }
                PromiseState::Pending => {
                    return Err(ClosedFailure::error(
                        JsErrorCode::JobLimit,
                        DiagnosticStage::JobDrain,
                        role,
                    ));
                }
            };
        }
        convert_value(&ctx, value, clone, string_gate, deadline, interrupted, role)
    })
}

fn convert_value<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    clone: Persistent<Function<'static>>,
    string_gate: Persistent<Function<'static>>,
    deadline: Instant,
    interrupted: &AtomicBool,
    role: ScriptRole,
) -> Result<StepOutcome, ClosedFailure> {
    if value.is_undefined() || value.is_null() {
        return Ok(StepOutcome::Void);
    }
    if value.is_string() {
        let string_gate = string_gate.restore(ctx).map_err(|error| {
            classify_ctx_error(
                ctx,
                error,
                deadline,
                interrupted,
                DiagnosticStage::ResultConversion,
                role,
            )
        })?;
        let bounded = string_gate.call::<_, String>((value,)).map_err(|error| {
            let failure = classify_ctx_error(
                ctx,
                error,
                deadline,
                interrupted,
                DiagnosticStage::ResultConversion,
                role,
            );
            match failure.outcome {
                StepOutcome::Timeout | StepOutcome::OutOfMemory => failure,
                _ => ClosedFailure::error(
                    JsErrorCode::InvalidResult,
                    DiagnosticStage::ResultConversion,
                    role,
                ),
            }
        })?;
        return Ok(StepOutcome::Value(bounded));
    }
    let primitive = if let Some(value) = value.as_int() {
        Some(value.to_string())
    } else if let Some(value) = value.as_float() {
        value.is_finite().then(|| value.to_string())
    } else {
        value.as_bool().map(|value| value.to_string())
    };
    if let Some(primitive) = primitive {
        return if primitive.len() <= MAX_RESULT_BYTES {
            Ok(StepOutcome::Value(primitive))
        } else {
            Err(ClosedFailure::error(
                JsErrorCode::InvalidResult,
                DiagnosticStage::ResultConversion,
                role,
            ))
        };
    }
    if !value.is_object() {
        return Err(ClosedFailure::error(
            JsErrorCode::InvalidResult,
            DiagnosticStage::ResultConversion,
            role,
        ));
    }
    let clone = clone.restore(ctx).map_err(|error| {
        classify_ctx_error(
            ctx,
            error,
            deadline,
            interrupted,
            DiagnosticStage::ResultConversion,
            role,
        )
    })?;
    let encoded = clone.call::<_, String>((value,)).map_err(|error| {
        let failure = classify_ctx_error(
            ctx,
            error,
            deadline,
            interrupted,
            DiagnosticStage::ResultConversion,
            role,
        );
        match failure.outcome {
            StepOutcome::Timeout | StepOutcome::OutOfMemory => failure,
            _ => ClosedFailure::error(
                JsErrorCode::InvalidResult,
                DiagnosticStage::ResultConversion,
                role,
            ),
        }
    })?;
    if encoded.len() > MAX_RESULT_BYTES {
        return Err(ClosedFailure::error(
            JsErrorCode::InvalidResult,
            DiagnosticStage::ResultConversion,
            role,
        ));
    }
    Ok(StepOutcome::Value(encoded))
}

fn initialization_failure(error: Error, role: ScriptRole) -> ClosedFailure {
    if matches!(error, Error::Allocation) {
        ClosedFailure::out_of_memory(DiagnosticStage::Initialization, role)
    } else {
        ClosedFailure::error(JsErrorCode::Internal, DiagnosticStage::Initialization, role)
    }
}

fn classify_error(
    context: &Context,
    error: Error,
    deadline: Instant,
    interrupted: &AtomicBool,
    stage: DiagnosticStage,
    role: ScriptRole,
) -> ClosedFailure {
    context.with(|ctx| classify_ctx_error(&ctx, error, deadline, interrupted, stage, role))
}

fn classify_ctx_error(
    ctx: &Ctx<'_>,
    error: Error,
    deadline: Instant,
    interrupted: &AtomicBool,
    stage: DiagnosticStage,
    role: ScriptRole,
) -> ClosedFailure {
    if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
        if matches!(error, Error::Exception) {
            let _ = ctx.catch();
        }
        return ClosedFailure::timeout(stage, role);
    }
    if matches!(error, Error::Allocation) {
        return ClosedFailure::out_of_memory(stage, role);
    }
    if !matches!(error, Error::Exception) {
        return ClosedFailure::error(JsErrorCode::Internal, stage, role);
    }

    let _ = ctx.catch();
    ClosedFailure::error(JsErrorCode::Exception, stage, role)
}

fn classify_evaluation_error(
    context: &Context,
    runtime: &Runtime,
    error: Error,
    deadline: Instant,
    interrupted: &AtomicBool,
    stage: DiagnosticStage,
    role: ScriptRole,
) -> ClosedFailure {
    let near_heap_limit = runtime_is_near_heap_limit(runtime);
    context.with(|ctx| {
        if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
            if matches!(error, Error::Exception) {
                let _ = ctx.catch();
            }
            return ClosedFailure::timeout(stage, role);
        }
        if matches!(error, Error::Allocation) || near_heap_limit {
            return ClosedFailure::out_of_memory(stage, role);
        }
        if !matches!(error, Error::Exception) {
            return ClosedFailure::error(JsErrorCode::Internal, stage, role);
        }
        let _ = ctx.catch();
        ClosedFailure::error(JsErrorCode::Exception, stage, role)
    })
}

fn runtime_is_near_heap_limit(runtime: &Runtime) -> bool {
    let usage = runtime.memory_usage();
    usage.malloc_size >= (MEMORY_LIMIT.saturating_sub(1024 * 1024)) as i64
}

#[cfg(feature = "skills")]
fn execute_verification(request: VerifyArtifact, limits: ExecutionLimits) -> VerificationResult {
    if request.cases.is_empty()
        || request.cases.len() > MAX_VERIFICATION_CASES
        || request.cases.iter().any(|case| {
            case.case_id.is_empty()
                || case.case_id.len() > MAX_VERIFICATION_CASE_ID_BYTES
                || case.script.len() > MAX_RESULT_BYTES
        })
    {
        return VerificationResult {
            passed: false,
            cases: Vec::new(),
            loader_version: VERIFICATION_LOADER_VERSION,
        };
    }
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(_) => return failed_skill_verification(&request, DiagnosticClass::Internal),
    };
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + limits.timeout;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupt_flag = interrupted.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let expired = Instant::now() >= deadline;
        if expired {
            interrupt_flag.store(true, Ordering::Relaxed);
        }
        expired
    })));

    let mut results = Vec::with_capacity(request.cases.len());
    let transcript_budget = super::skills::fakes::VerificationTranscriptBudget::new();
    let mut transcript_calls_remaining = super::skills::fakes::VERIFICATION_TRANSCRIPT_MAX_CALLS;
    let mut terminal = None;
    for (case_index, case) in request.cases.iter().enumerate() {
        if let Some(diagnostic) = terminal.clone() {
            results.push(failed_case(case.case_id.clone(), diagnostic));
            continue;
        }
        if runtime.is_job_pending() {
            let diagnostic = diagnostic(
                DiagnosticClass::Contract,
                DiagnosticStage::JobDrain,
                verification_case_role(&case.kind),
            );
            terminal = Some(diagnostic.clone());
            results.push(failed_case(case.case_id.clone(), diagnostic));
            continue;
        }
        let mut result = execute_isolated_skill_verification_case(
            &runtime,
            &request.artifact,
            case,
            case_index,
            deadline,
            &interrupted,
            limits.max_pending_jobs,
            transcript_budget.clone(),
        );
        if transcript_budget.exceeded() {
            let limit_diagnostic = diagnostic(
                DiagnosticClass::Contract,
                DiagnosticStage::Verification,
                verification_case_role(&case.kind),
            );
            result = failed_case(case.case_id.clone(), limit_diagnostic.clone());
            terminal = Some(limit_diagnostic);
        }
        if result.transcript.call_count() > transcript_calls_remaining {
            if result.passed {
                result = failed_case(
                    case.case_id.clone(),
                    diagnostic(
                        DiagnosticClass::Contract,
                        DiagnosticStage::Verification,
                        verification_case_role(&case.kind),
                    ),
                );
            }
            result
                .transcript
                .limit_call_count(&mut transcript_calls_remaining);
        } else {
            transcript_calls_remaining -= result.transcript.call_count();
        }
        if result.diagnostic.as_ref().is_some_and(|diagnostic| {
            diagnostic.class == DiagnosticClass::ResourceLimit
                || diagnostic.stage == DiagnosticStage::JobDrain
        }) {
            terminal = result.diagnostic.clone();
        }
        results.push(result);
    }
    VerificationResult {
        passed: results.iter().all(|case| case.passed),
        cases: results,
        loader_version: VERIFICATION_LOADER_VERSION,
    }
}

#[cfg(feature = "skills")]
fn verification_case_role(kind: &super::protocol::VerificationCaseKind) -> ScriptRole {
    use super::protocol::VerificationCaseKind;
    match kind {
        VerificationCaseKind::Embedded => ScriptRole::EmbeddedTest,
        VerificationCaseKind::Mutation { .. } => ScriptRole::MutationTest,
        VerificationCaseKind::Inherited => ScriptRole::InheritedTest,
        VerificationCaseKind::HeldOut { .. } => ScriptRole::HeldOutTest,
    }
}

#[cfg(feature = "skills")]
#[allow(clippy::too_many_arguments)]
fn execute_isolated_skill_verification_case(
    runtime: &Runtime,
    artifact: &super::skills::SkillArtifact,
    case: &super::protocol::VerificationCase,
    case_index: usize,
    deadline: Instant,
    interrupted: &AtomicBool,
    max_pending_jobs: usize,
    transcript_budget: super::skills::fakes::VerificationTranscriptBudget,
) -> VerificationCaseResult {
    use super::protocol::VerificationCaseKind;
    use super::skills::fakes::{FakeHostGlobals, FakeTranscript};

    let role = verification_case_role(&case.kind);
    let fakes =
        FakeHostGlobals::with_transcript_budget(artifact.capability.clone(), transcript_budget);
    if let VerificationCaseKind::HeldOut { fake_files, .. } = &case.kind
        && (fake_files.len() > 32
            || fake_files.iter().any(|(path, contents)| {
                path.is_empty() || path.len() > 4 * 1024 || contents.len() > 64 * 1024
            })
            || fake_files
                .iter()
                .any(|(path, contents)| fakes.seed_file(path, contents).is_err()))
    {
        return VerificationCaseResult {
            case_id: case.case_id.clone(),
            passed: false,
            diagnostic: Some(diagnostic(
                DiagnosticClass::Contract,
                DiagnosticStage::Initialization,
                role,
            )),
            transcript: FakeTranscript::default(),
        };
    }
    let dispatch_fakes = fakes.clone();
    let manifest = artifact.capability.clone();
    let capabilities = InvocationCapabilityRuntime::new(move |effect| {
        execute_verification_fake(&manifest, &dispatch_fakes, effect.request.operation)
    });
    let bindings = match prepare_verification_bindings(artifact, &capabilities, case_index) {
        Ok(bindings) => bindings,
        Err(()) => {
            return VerificationCaseResult {
                case_id: case.case_id.clone(),
                passed: false,
                diagnostic: Some(diagnostic(
                    DiagnosticClass::Internal,
                    DiagnosticStage::Initialization,
                    role,
                )),
                transcript: fakes.transcript().bounded_for_wire(),
            };
        }
    };
    let context = match Context::full(runtime) {
        Ok(context) => context,
        Err(_) => {
            return VerificationCaseResult {
                case_id: case.case_id.clone(),
                passed: false,
                diagnostic: Some(diagnostic(
                    DiagnosticClass::Internal,
                    DiagnosticStage::Initialization,
                    role,
                )),
                transcript: fakes.transcript().bounded_for_wire(),
            };
        }
    };
    let mutated_export = match &case.kind {
        VerificationCaseKind::Mutation { export_name } => Some(export_name.as_str()),
        _ => None,
    };
    let loaded = match super::realm::load_artifact_with_bound_exports_for_verification(
        runtime,
        &context,
        artifact,
        capabilities.clone(),
        bindings,
        mutated_export,
    ) {
        Ok(loaded) => loaded,
        Err(error) => {
            let class = match error {
                super::realm::RealmError::Identity
                | super::realm::RealmError::InvalidExport
                | super::realm::RealmError::DuplicateExport
                | super::realm::RealmError::ExportCollision
                | super::realm::RealmError::MissingExport
                | super::realm::RealmError::PendingInitializationJobs => DiagnosticClass::Contract,
                super::realm::RealmError::Initialization
                | super::realm::RealmError::PrivateLibraryCompilation
                | super::realm::RealmError::PrivateLibraryBytecodeLoad
                | super::realm::RealmError::PrivateLibraryModuleEvaluation
                | super::realm::RealmError::PrivateLibraryExportLookup
                | super::realm::RealmError::PrivateLibraryFactoryExecution
                | super::realm::RealmError::WrapperInstallation => DiagnosticClass::Exception,
            };
            return VerificationCaseResult {
                case_id: case.case_id.clone(),
                passed: false,
                diagnostic: Some(diagnostic(
                    class,
                    DiagnosticStage::Initialization,
                    ScriptRole::SkillSource,
                )),
                transcript: fakes.transcript().bounded_for_wire(),
            };
        }
    };
    let mut remaining_jobs = max_pending_jobs;
    let mut result = match &case.kind {
        VerificationCaseKind::Embedded | VerificationCaseKind::Inherited => {
            execute_verification_case(
                runtime,
                &context,
                case.case_id.clone(),
                &case.script,
                role,
                deadline,
                interrupted,
                &mut remaining_jobs,
            )
            .0
        }
        VerificationCaseKind::HeldOut { expected, .. } => execute_held_out_verification_case(
            runtime,
            &context,
            case,
            expected,
            deadline,
            interrupted,
            &mut remaining_jobs,
        ),
        VerificationCaseKind::Mutation { .. } => execute_mutation_verification_case(
            runtime,
            &context,
            artifact,
            &case.case_id,
            deadline,
            interrupted,
            &mut remaining_jobs,
        ),
    };
    drop(loaded);
    drop(context);
    if runtime.is_job_pending() && result.passed {
        result = failed_case(
            case.case_id.clone(),
            diagnostic(DiagnosticClass::Contract, DiagnosticStage::JobDrain, role),
        );
    }
    let transcript = fakes.transcript();
    if transcript.exceeds_wire_call_limit() && result.passed {
        result = failed_case(
            case.case_id.clone(),
            diagnostic(
                DiagnosticClass::Contract,
                DiagnosticStage::Verification,
                role,
            ),
        );
    }
    result.transcript = transcript.bounded_for_wire();
    result
}

#[cfg(feature = "skills")]
fn prepare_verification_bindings(
    artifact: &super::skills::SkillArtifact,
    capabilities: &InvocationCapabilityRuntime,
    case_index: usize,
) -> Result<std::collections::HashMap<String, super::realm::BoundExportInvocation>, ()> {
    use std::collections::HashMap;

    let mut bindings = HashMap::with_capacity(artifact.exports.len());
    for (export_index, export) in artifact.exports.iter().enumerate() {
        let capabilities = capabilities.clone();
        let artifact_id = artifact.id.clone();
        let export_name = export.name.clone();
        let manifest = artifact.capability.clone();
        let authorize = Arc::new(move |call_ordinal: u32| {
            let invocation = format!("verify-{case_index}-{export_index}-{call_ordinal}");
            let invocation_id =
                super::protocol::InvocationId::new(invocation.clone()).map_err(|_| ())?;
            let authorization = InvocationAuthorization::new(
                invocation_id,
                artifact_id.clone(),
                export_name.clone(),
                manifest.clone(),
                manifest.grants.iter().map(|scope| {
                    (
                        scope.capability(),
                        super::protocol::GrantId::new(uuid::Uuid::new_v4())
                            .expect("random verification grant is non-nil"),
                    )
                }),
            )
            .map_err(|_| ())?;
            let handle = capabilities.prepare(authorization).map_err(|_| ())?;
            Ok((handle, invocation))
        });
        bindings.insert(
            export.name.clone(),
            super::realm::BoundExportInvocation {
                authorize,
                on_start: Arc::new(|_, _| Ok(())),
                on_terminal: Arc::new(|_, _| Ok(())),
            },
        );
    }
    Ok(bindings)
}

#[cfg(feature = "skills")]
fn execute_verification_fake(
    manifest: &super::skills::CapabilityManifest,
    fakes: &super::skills::fakes::FakeHostGlobals,
    operation: EffectOperation,
) -> Result<EffectResult, super::skills::capability::CapabilityError> {
    use super::skills::capability::CapabilityError;
    if !verification_scope_allows(manifest, &operation) {
        return Err(CapabilityError::DispatchDenied);
    }
    match operation {
        EffectOperation::ReadFile { path } => fakes
            .read_file(&path)
            .map(|content| EffectResult::ReadFile { content })
            .map_err(|_| CapabilityError::DispatchDenied),
        EffectOperation::WriteFile { path, content } => fakes
            .write_file(&path, &content)
            .map(|()| EffectResult::WriteFile)
            .map_err(|_| CapabilityError::DispatchDenied),
        EffectOperation::Spawn { program, arguments } => fakes
            .spawn(&program, &arguments)
            .map(|_| EffectResult::Spawn {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })
            .map_err(|_| CapabilityError::DispatchDenied),
        EffectOperation::Fetch { url, method, .. } => {
            let method = match method {
                super::protocol::HttpMethod::Get => "GET",
                super::protocol::HttpMethod::Post => "POST",
            };
            fakes
                .fetch(&url, method)
                .map(|body| EffectResult::Fetch { status: 200, body })
                .map_err(|_| CapabilityError::DispatchDenied)
        }
        EffectOperation::ProposeSkill { .. } => Err(CapabilityError::DispatchDenied),
    }
}

#[cfg(feature = "skills")]
fn verification_scope_allows(
    manifest: &super::skills::CapabilityManifest,
    operation: &EffectOperation,
) -> bool {
    use super::skills::{CapabilityScope, HostCapability, HttpMethod as SkillHttpMethod};
    match operation {
        EffectOperation::ReadFile { path } => manifest
            .scope(HostCapability::ReadFile)
            .and_then(|scope| match scope {
                CapabilityScope::ReadFile { workspace_prefixes } => Some(workspace_prefixes),
                _ => None,
            })
            .is_some_and(|prefixes| {
                prefixes
                    .iter()
                    .any(|prefix| virtual_path_in_scope(prefix, path))
            }),
        EffectOperation::WriteFile { path, .. } => manifest
            .scope(HostCapability::WriteFile)
            .and_then(|scope| match scope {
                CapabilityScope::WriteFile { workspace_prefixes } => Some(workspace_prefixes),
                _ => None,
            })
            .is_some_and(|prefixes| {
                prefixes
                    .iter()
                    .any(|prefix| virtual_path_in_scope(prefix, path))
            }),
        EffectOperation::Spawn { program, .. } => manifest
            .scope(HostCapability::Spawn)
            .and_then(|scope| match scope {
                CapabilityScope::Spawn { programs } => Some(programs),
                _ => None,
            })
            .is_some_and(|programs| programs.contains(program)),
        EffectOperation::Fetch { url, method, .. } => {
            let Ok(url) = reqwest::Url::parse(url) else {
                return false;
            };
            let origin = url.origin().ascii_serialization();
            manifest
                .scope(HostCapability::Fetch)
                .and_then(|scope| match scope {
                    CapabilityScope::Fetch { origins, methods } => Some((origins, methods)),
                    _ => None,
                })
                .is_some_and(|(origins, methods)| {
                    origins.contains(&origin)
                        && methods.iter().any(|allowed| {
                            matches!(
                                (allowed, method),
                                (SkillHttpMethod::Get, super::protocol::HttpMethod::Get)
                                    | (SkillHttpMethod::Post, super::protocol::HttpMethod::Post)
                            )
                        })
                })
        }
        EffectOperation::ProposeSkill { .. } => false,
    }
}

#[cfg(feature = "skills")]
fn virtual_path_in_scope(prefix: &str, path: &str) -> bool {
    !path.starts_with('/')
        && !path.split('/').any(|component| component == "..")
        && (path == prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('/')))
}

#[cfg(feature = "skills")]
#[allow(clippy::too_many_arguments)]
fn execute_held_out_verification_case(
    runtime: &Runtime,
    context: &Context,
    case: &super::protocol::VerificationCase,
    expected: &super::protocol::VerificationExpectedValue,
    deadline: Instant,
    interrupted: &AtomicBool,
    remaining_jobs: &mut usize,
) -> VerificationCaseResult {
    let role = ScriptRole::HeldOutTest;
    let outcome =
        evaluate(context, &case.script, runtime, deadline, interrupted, role).and_then(|value| {
            drain_jobs(runtime, deadline, interrupted, remaining_jobs, role)?;
            context.with(|ctx| {
                let value = value.restore(&ctx).map_err(|error| {
                    classify_ctx_error(
                        &ctx,
                        error,
                        deadline,
                        interrupted,
                        DiagnosticStage::Verification,
                        role,
                    )
                })?;
                verification_expected_matches(expected, &value)
                    .then_some(())
                    .ok_or_else(|| {
                        ClosedFailure::error(
                            JsErrorCode::InvalidResult,
                            DiagnosticStage::Verification,
                            role,
                        )
                    })
            })
        });
    match outcome {
        Ok(()) => VerificationCaseResult {
            case_id: case.case_id.clone(),
            passed: true,
            diagnostic: None,
            transcript: Default::default(),
        },
        Err(failure) => failed_case(case.case_id.clone(), failure.diagnostic),
    }
}

#[cfg(feature = "skills")]
fn verification_expected_matches(
    expected: &super::protocol::VerificationExpectedValue,
    actual: &Value<'_>,
) -> bool {
    use super::protocol::VerificationExpectedValue;
    match expected {
        VerificationExpectedValue::Boolean(expected) => actual.as_bool() == Some(*expected),
        VerificationExpectedValue::String(expected) => actual
            .as_string()
            .and_then(|value| value.to_string().ok())
            .is_some_and(|actual| actual == *expected),
        VerificationExpectedValue::Integer(expected) => actual
            .as_int()
            .is_some_and(|actual| i64::from(actual) == *expected),
        VerificationExpectedValue::Float(expected) => actual
            .as_float()
            .or_else(|| actual.as_int().map(f64::from))
            .is_some_and(|actual| actual == *expected),
        VerificationExpectedValue::Null => actual.is_null(),
    }
}

#[cfg(feature = "skills")]
#[allow(clippy::too_many_arguments)]
fn execute_mutation_verification_case(
    runtime: &Runtime,
    context: &Context,
    artifact: &super::skills::SkillArtifact,
    case_id: &str,
    deadline: Instant,
    interrupted: &AtomicBool,
    remaining_jobs: &mut usize,
) -> VerificationCaseResult {
    for test in &artifact.tests {
        let (result, terminal) = execute_verification_case(
            runtime,
            context,
            case_id.to_string(),
            test,
            ScriptRole::MutationTest,
            deadline,
            interrupted,
            remaining_jobs,
        );
        if !result.passed {
            if terminal {
                return result;
            }
            return VerificationCaseResult {
                case_id: case_id.to_string(),
                passed: true,
                diagnostic: None,
                transcript: Default::default(),
            };
        }
    }
    failed_case(
        case_id.to_string(),
        diagnostic(
            DiagnosticClass::Contract,
            DiagnosticStage::Verification,
            ScriptRole::MutationTest,
        ),
    )
}

#[cfg(feature = "skills")]
fn failed_skill_verification(
    request: &VerifyArtifact,
    class: DiagnosticClass,
) -> VerificationResult {
    let diagnostic = diagnostic(
        class,
        DiagnosticStage::Initialization,
        ScriptRole::SkillSource,
    );
    VerificationResult {
        passed: false,
        cases: request
            .cases
            .iter()
            .map(|case| failed_case(case.case_id.clone(), diagnostic.clone()))
            .collect(),
        loader_version: VERIFICATION_LOADER_VERSION,
    }
}

#[cfg(not(feature = "skills"))]
fn execute_verification(request: VerifyArtifact, limits: ExecutionLimits) -> VerificationResult {
    let case_count = request
        .artifact
        .tests
        .len()
        .saturating_add(request.cases.len());
    if case_count > MAX_VERIFICATION_CASES
        || request
            .cases
            .iter()
            .any(|case| case.case_id.len() > MAX_VERIFICATION_CASE_ID_BYTES)
    {
        return VerificationResult {
            passed: false,
            cases: Vec::new(),
            loader_version: VERIFICATION_LOADER_VERSION,
        };
    }
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(_) => return failed_verification(&request, DiagnosticClass::Internal),
    };
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + limits.timeout;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupt_flag = interrupted.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let expired = Instant::now() >= deadline;
        if expired {
            interrupt_flag.store(true, Ordering::Relaxed);
        }
        expired
    })));
    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(_) => return failed_verification(&request, DiagnosticClass::Internal),
    };
    if context
        .with(|ctx| {
            Object::new(ctx.clone()).and_then(|exports| ctx.globals().set("exports", exports))
        })
        .is_err()
    {
        return failed_verification(&request, DiagnosticClass::Internal);
    }

    let mut remaining_jobs = limits.max_pending_jobs;
    let source = evaluate(
        &context,
        &request.artifact.source,
        &runtime,
        deadline,
        &interrupted,
        ScriptRole::SkillSource,
    )
    .and_then(|value| {
        drain_jobs(
            &runtime,
            deadline,
            &interrupted,
            &mut remaining_jobs,
            ScriptRole::SkillSource,
        )?;
        ensure_source_settled(&runtime, &context, value, deadline, &interrupted)
    });
    if let Err(failure) = source {
        return failed_verification_with(&request, failure.diagnostic);
    }

    let mut cases = Vec::with_capacity(request.artifact.tests.len() + request.cases.len());
    let mut terminal_diagnostic: Option<Diagnostic> = None;
    for (index, script) in request.artifact.tests.iter().enumerate() {
        let case_id = format!("embedded-{index}");
        if let Some(diagnostic) = &terminal_diagnostic {
            cases.push(failed_case(case_id, diagnostic.clone()));
        } else {
            let (case, terminal) = execute_verification_case(
                &runtime,
                &context,
                case_id,
                script,
                ScriptRole::EmbeddedTest,
                deadline,
                &interrupted,
                &mut remaining_jobs,
            );
            if terminal {
                terminal_diagnostic = case.diagnostic.clone();
            }
            cases.push(case);
        }
    }
    for case in &request.cases {
        if let Some(diagnostic) = &terminal_diagnostic {
            cases.push(failed_case(case.case_id.clone(), diagnostic.clone()));
        } else {
            let (result, terminal) = execute_verification_case(
                &runtime,
                &context,
                case.case_id.clone(),
                &case.script,
                ScriptRole::HeldOutTest,
                deadline,
                &interrupted,
                &mut remaining_jobs,
            );
            if terminal {
                terminal_diagnostic = result.diagnostic.clone();
            }
            cases.push(result);
        }
    }
    VerificationResult {
        passed: cases.iter().all(|case| case.passed),
        cases,
        loader_version: VERIFICATION_LOADER_VERSION,
    }
}

#[cfg(not(feature = "skills"))]
fn ensure_source_settled(
    runtime: &Runtime,
    context: &Context,
    value: Persistent<Value<'static>>,
    deadline: Instant,
    interrupted: &AtomicBool,
) -> Result<(), ClosedFailure> {
    let near_heap_limit = runtime_is_near_heap_limit(runtime);
    context.with(|ctx| {
        let value = value.restore(&ctx).map_err(|error| {
            classify_ctx_error(
                &ctx,
                error,
                deadline,
                interrupted,
                DiagnosticStage::Verification,
                ScriptRole::SkillSource,
            )
        })?;
        let Some(promise) = value.as_promise() else {
            return Ok(());
        };
        match promise.state() {
            PromiseState::Resolved => match promise.result::<Value>() {
                Some(Ok(_)) => Ok(()),
                _ => Err(ClosedFailure::error(
                    JsErrorCode::Internal,
                    DiagnosticStage::Verification,
                    ScriptRole::SkillSource,
                )),
            },
            PromiseState::Rejected => {
                let _ = promise.result::<Value>();
                let _ = ctx.catch();
                if near_heap_limit {
                    Err(ClosedFailure::out_of_memory(
                        DiagnosticStage::Verification,
                        ScriptRole::SkillSource,
                    ))
                } else {
                    Err(ClosedFailure::error(
                        JsErrorCode::Exception,
                        DiagnosticStage::Verification,
                        ScriptRole::SkillSource,
                    ))
                }
            }
            PromiseState::Pending => Err(ClosedFailure::error(
                JsErrorCode::JobLimit,
                DiagnosticStage::JobDrain,
                ScriptRole::SkillSource,
            )),
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_verification_case(
    runtime: &Runtime,
    context: &Context,
    case_id: String,
    script: &str,
    role: ScriptRole,
    deadline: Instant,
    interrupted: &AtomicBool,
    remaining_jobs: &mut usize,
) -> (VerificationCaseResult, bool) {
    let result =
        evaluate(context, script, runtime, deadline, interrupted, role).and_then(|value| {
            drain_jobs(runtime, deadline, interrupted, remaining_jobs, role)?;
            let near_heap_limit = runtime_is_near_heap_limit(runtime);
            context.with(|ctx| {
                let mut value = value.restore(&ctx).map_err(|error| {
                    classify_ctx_error(
                        &ctx,
                        error,
                        deadline,
                        interrupted,
                        DiagnosticStage::Verification,
                        role,
                    )
                })?;
                if let Some(promise) = value.as_promise() {
                    value = match promise.state() {
                        PromiseState::Resolved => promise
                            .result::<Value>()
                            .and_then(Result::ok)
                            .ok_or_else(|| {
                                ClosedFailure::error(
                                    JsErrorCode::Internal,
                                    DiagnosticStage::Verification,
                                    role,
                                )
                            })?,
                        PromiseState::Rejected => {
                            let _ = promise.result::<Value>();
                            let _ = ctx.catch();
                            if near_heap_limit {
                                return Err(ClosedFailure::out_of_memory(
                                    DiagnosticStage::Verification,
                                    role,
                                ));
                            }
                            return Err(ClosedFailure::error(
                                JsErrorCode::Exception,
                                DiagnosticStage::Verification,
                                role,
                            ));
                        }
                        PromiseState::Pending => {
                            return Err(ClosedFailure::error(
                                JsErrorCode::JobLimit,
                                DiagnosticStage::JobDrain,
                                role,
                            ));
                        }
                    };
                }
                if value.as_bool() == Some(true) {
                    Ok(())
                } else {
                    Err(ClosedFailure::error(
                        JsErrorCode::InvalidResult,
                        DiagnosticStage::Verification,
                        role,
                    ))
                }
            })
        });
    match result {
        Ok(()) => (
            VerificationCaseResult {
                case_id,
                passed: true,
                diagnostic: None,
                #[cfg(feature = "skills")]
                transcript: Default::default(),
            },
            false,
        ),
        Err(failure) => {
            let terminal = matches!(
                failure.outcome,
                StepOutcome::Timeout
                    | StepOutcome::OutOfMemory
                    | StepOutcome::Error(JsErrorCode::JobLimit)
            );
            (failed_case(case_id, failure.diagnostic), terminal)
        }
    }
}

fn failed_case(case_id: String, diagnostic: Diagnostic) -> VerificationCaseResult {
    VerificationCaseResult {
        case_id,
        passed: false,
        diagnostic: Some(diagnostic),
        #[cfg(feature = "skills")]
        transcript: Default::default(),
    }
}

#[cfg(not(feature = "skills"))]
fn failed_verification(request: &VerifyArtifact, class: DiagnosticClass) -> VerificationResult {
    failed_verification_with(
        request,
        diagnostic(
            class,
            DiagnosticStage::Initialization,
            ScriptRole::SkillSource,
        ),
    )
}

#[cfg(not(feature = "skills"))]
fn failed_verification_with(
    request: &VerifyArtifact,
    diagnostic: Diagnostic,
) -> VerificationResult {
    let mut cases = request
        .artifact
        .tests
        .iter()
        .enumerate()
        .map(|(index, _)| VerificationCaseResult {
            case_id: format!("embedded-{index}"),
            passed: false,
            diagnostic: Some(diagnostic.clone()),
        })
        .collect::<Vec<_>>();
    cases.extend(request.cases.iter().map(|case| VerificationCaseResult {
        case_id: case.case_id.clone(),
        passed: false,
        diagnostic: Some(diagnostic.clone()),
    }));
    VerificationResult {
        passed: false,
        cases,
        loader_version: VERIFICATION_LOADER_VERSION,
    }
}

#[cfg(test)]
pub(crate) fn exit_test_worker() -> ! {
    std::process::exit(run_marked_worker())
}
