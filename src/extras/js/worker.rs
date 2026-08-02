//! Synchronous bootstrap and fresh-runtime execution for the brokered JavaScript worker.
//!
//! Every request owns its QuickJS [`Runtime`] and [`Context`]. Neither is stored in worker state,
//! and every JavaScript value is converted to a bounded, closed Rust protocol value before the
//! terminal frame is written. The only global installed here is a bounded `console`; authority
//! globals and module loaders are deliberately absent.

use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::promise::PromiseState;
use rquickjs::{Context, Ctx, Error, Function, Object, Persistent, Runtime, Value};

use super::protocol::{
    BuildIdentity, ConsoleLevel, ConsoleRecord, Diagnostic, DiagnosticClass, DiagnosticStage,
    JsErrorCode, ParentFrame, ParentWireFrame, RunStep, ScriptRole, StepOutcome, StepResult,
    VerificationCaseResult, VerificationResult, VerifyArtifact, WireFrame, WorkerFrame,
    WorkerProtocol, WorkerReady, WorkerWireFrame, read_frame, write_frame,
};
use super::types::{MEMORY_LIMIT, STACK_LIMIT, STEP_TIMEOUT};
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

const STRICT_CLONE_SOURCE: &str = r#"
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
            return Self {
                timeout,
                max_pending_jobs,
            };
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
        line: None,
        column: None,
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

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    if bootstrap(&mut input, &mut output).is_ok() {
        0
    } else {
        EXIT_FAILURE
    }
}

fn bootstrap(input: &mut impl std::io::Read, output: &mut impl Write) -> Result<(), ()> {
    let build = BuildIdentity::current();
    let limits = ExecutionLimits::current();
    let mut protocol = WorkerProtocol::new(build.clone());

    let hello: ParentWireFrame = read_frame(input).map_err(|_| ())?;
    if !matches!(hello.message, ParentFrame::Hello(_)) {
        return Err(());
    }
    protocol.on_receive(&hello).map_err(|_| ())?;

    finalize_internal_worker().map_err(|_| ())?;

    let ready: WorkerWireFrame =
        WireFrame::connection(build.clone(), 1, WorkerFrame::Ready(WorkerReady {}));
    protocol.on_send(&ready).map_err(|_| ())?;
    write_terminal(output, &ready)?;

    loop {
        let request: ParentWireFrame = read_frame(input).map_err(|_| ())?;
        protocol.on_receive(&request).map_err(|_| ())?;
        let invocation_id = request.invocation_id.clone();
        let sequence = request.sequence.checked_add(1).ok_or(())?;
        let message = match request.message {
            ParentFrame::RunStep(step) => WorkerFrame::StepResult(execute_run_step(step, limits)),
            ParentFrame::VerifyArtifact(request) => {
                WorkerFrame::VerificationResult(execute_verification(request, limits))
            }
            ParentFrame::Shutdown => return Ok(()),
            ParentFrame::Hello(_) | ParentFrame::EffectResponse(_) => return Err(()),
        };
        let response = WireFrame {
            protocol_version: super::protocol::PROTOCOL_VERSION,
            build_id: build.clone(),
            invocation_id,
            sequence,
            message,
        };
        protocol.on_send(&response).map_err(|_| ())?;
        write_terminal(output, &response)?;
    }
}

fn write_terminal(output: &mut impl Write, frame: &WorkerWireFrame) -> Result<(), ()> {
    write_frame(output, frame).map_err(|_| ())?;
    output.flush().map_err(|_| ())
}

fn execute_run_step(request: RunStep, limits: ExecutionLimits) -> StepResult {
    let console = Arc::new(Mutex::new(Vec::new()));
    let execution = execute_fresh_step(&request.code, ScriptRole::Model, limits, console.clone());
    let console = console
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    match execution {
        Ok(outcome) => StepResult {
            outcome,
            console,
            diagnostic: None,
        },
        Err(failure) => StepResult {
            outcome: failure.outcome,
            console,
            diagnostic: Some(failure.diagnostic),
        },
    }
}

fn execute_fresh_step(
    source: &str,
    role: ScriptRole,
    limits: ExecutionLimits,
    console: Arc<Mutex<Vec<ConsoleRecord>>>,
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
    let clone = context
        .with(|ctx| {
            ctx.eval::<Function, _>(STRICT_CLONE_SOURCE)
                .map(|function| Persistent::save(&ctx, function))
        })
        .map_err(|error| {
            classify_error(
                &context,
                error,
                deadline,
                &interrupted,
                DiagnosticStage::Initialization,
                role,
            )
        })?;
    let string_gate = context
        .with(|ctx| {
            ctx.eval::<Function, _>(STRING_GATE_SOURCE)
                .map(|function| Persistent::save(&ctx, function))
        })
        .map_err(|error| {
            classify_error(
                &context,
                error,
                deadline,
                &interrupted,
                DiagnosticStage::Initialization,
                role,
            )
        })?;
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
    }
}

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
