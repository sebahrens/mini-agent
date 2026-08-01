use rquickjs::context::EvalOptions;
use rquickjs::promise::PromiseState;
use rquickjs::{Coerced, Context, Ctx, Error, FromJs, Persistent, Runtime, Value};
#[cfg(feature = "skills")]
use rquickjs::{Function, Object};
#[cfg(feature = "skills")]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "skills")]
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
#[cfg(feature = "skills")]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

use crate::extras::js::host::{AllowConfig, register_host_globals};
#[cfg(feature = "skills")]
use crate::extras::js::host::{SkillCapabilityGate, register_proposal_global};
#[cfg(feature = "skills")]
use crate::extras::js::skills::proposal::ProposalHost;
use crate::extras::js::tool::PermissionBridge;
use crate::extras::js::types::*;
use crate::sandbox::Sandbox;

const MAX_PENDING_JOBS: usize = 10_000;

#[derive(Clone, Default)]
pub(crate) struct NormalExecutionHosts {
    #[cfg(feature = "skills")]
    proposal: Option<ProposalHost>,
}

impl NormalExecutionHosts {
    #[cfg(feature = "skills")]
    pub(crate) fn with_proposal(proposal: ProposalHost) -> Self {
        Self {
            proposal: Some(proposal),
        }
    }
}

#[derive(Clone, Copy)]
enum ReplyPath {
    EarlyCancel,
    AbandonedBeforeExecution,
    Completed,
}

impl ReplyPath {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EarlyCancel => "early_cancel",
            Self::AbandonedBeforeExecution => "abandoned_before_execution",
            Self::Completed => "completed",
        }
    }
}

fn log_reply_drop(reply_path: ReplyPath) {
    tracing::debug!(
        reply_path = reply_path.as_str(),
        "JS engine reply receiver dropped before response delivery"
    );
}

fn deliver_reply_or_cancel<T>(
    reply: oneshot::Sender<T>,
    response: T,
    cancellation: &PermCancellation,
    reply_path: ReplyPath,
) {
    // A closed receiver is expected when the caller's deadline wins the race.
    // Keep the diagnostic independent of response formatting traits and payload contents.
    if reply.send(response).is_err() {
        cancellation.cancel();
        log_reply_drop(reply_path);
    }
}

#[derive(Clone, Copy)]
struct ExecutionPolicy {
    timeout: Duration,
    max_pending_jobs: usize,
}

fn interruption_outcome(
    deadline: Instant,
    cancellation: &PermCancellation,
    permission_bridge: &PermissionBridge,
) -> Option<JsOutcome> {
    if Instant::now() >= deadline {
        Some(JsOutcome::Timeout)
    } else if cancellation.is_cancelled() {
        Some(JsOutcome::Error("execution cancelled".to_string()))
    } else if permission_bridge.is_shutdown() {
        Some(JsOutcome::Error("permission bridge shut down".to_string()))
    } else {
        None
    }
}

fn stringify_thrown_value<'a>(ctx: &Ctx<'a>, value: &Value<'a>) -> String {
    if value.is_null() {
        return "null".to_string();
    }
    if value.is_undefined() {
        return "undefined".to_string();
    }

    match Coerced::<String>::from_js(ctx, value.clone()) {
        Ok(value) => value.0,
        Err(_) => {
            // String coercion may itself throw (for example, a hostile toString).
            // Clear that secondary exception and return a stable, non-panicking fallback.
            let _ = ctx.catch();
            format!("<unstringifiable {}>", value.type_name())
        }
    }
}

fn thrown_value_outcome<'a>(
    ctx: &Ctx<'a>,
    value: Value<'a>,
    deadline: Instant,
    cancellation: &PermCancellation,
    permission_bridge: &PermissionBridge,
) -> JsOutcome {
    if let Some(outcome) = interruption_outcome(deadline, cancellation, permission_bridge) {
        return outcome;
    }

    if let Some(exception) = value.as_exception() {
        let message = exception.message().unwrap_or_default();
        let name = exception
            .as_object()
            .get::<_, Option<Coerced<String>>>("name")
            .ok()
            .flatten()
            .map(|name| name.0)
            .unwrap_or_default();

        // rquickjs 0.12 maps a JS_EXCEPTION return from eval to Error::Exception.
        // QuickJS-NG's JS_ThrowOutOfMemory creates exactly this InternalError
        // (quickjs.c: JS_ThrowOutOfMemory); there is no distinct public OOM tag.
        // Exact name/message matching avoids misclassifying unrelated errors that
        // merely mention memory, while Error::Allocation is handled separately.
        if name == "InternalError" && message == "out of memory" {
            return JsOutcome::OomKilled;
        }

        let stack = exception.stack().unwrap_or_default();
        return match (message.is_empty(), stack.is_empty()) {
            (false, false) => JsOutcome::Error(format!("{message}\n{stack}")),
            (false, true) => JsOutcome::Error(message),
            (true, false) => JsOutcome::Error(stack),
            (true, true) => JsOutcome::Error(stringify_thrown_value(ctx, &value)),
        };
    }

    JsOutcome::Error(stringify_thrown_value(ctx, &value))
}

fn error_outcome(
    ctx: &Ctx<'_>,
    error: Error,
    deadline: Instant,
    cancellation: &PermCancellation,
    permission_bridge: &PermissionBridge,
) -> JsOutcome {
    match error {
        Error::Allocation => JsOutcome::OomKilled,
        Error::Exception => {
            thrown_value_outcome(ctx, ctx.catch(), deadline, cancellation, permission_bridge)
        }
        error => JsOutcome::Error(error.to_string()),
    }
}

fn value_outcome(value: Value<'_>) -> JsOutcome {
    if value.is_undefined() || value.is_null() {
        JsOutcome::Void
    } else if let Some(value) = value.as_string() {
        match value.to_string() {
            Ok(value) => JsOutcome::Value(value),
            Err(error) => JsOutcome::Error(error.to_string()),
        }
    } else if let Some(value) = value.as_int() {
        JsOutcome::Value(value.to_string())
    } else if let Some(value) = value.as_float() {
        JsOutcome::Value(value.to_string())
    } else if let Some(value) = value.as_bool() {
        JsOutcome::Value(value.to_string())
    } else {
        JsOutcome::Value(format!("{value:?}"))
    }
}

fn settled_value_outcome(
    ctx: &Ctx<'_>,
    value: Value<'_>,
    deadline: Instant,
    cancellation: &PermCancellation,
    permission_bridge: &PermissionBridge,
) -> JsOutcome {
    let Some(promise) = value.as_promise() else {
        return value_outcome(value);
    };

    match promise.state() {
        PromiseState::Pending => JsOutcome::Error(
            "Promise remained pending after the JavaScript job queue drained".to_string(),
        ),
        PromiseState::Resolved => match promise.result::<Value>() {
            Some(Ok(value)) => value_outcome(value),
            Some(Err(error)) => {
                error_outcome(ctx, error, deadline, cancellation, permission_bridge)
            }
            None => JsOutcome::Error("Resolved Promise had no result".to_string()),
        },
        PromiseState::Rejected => match promise.result::<Value>() {
            Some(Err(error)) => {
                error_outcome(ctx, error, deadline, cancellation, permission_bridge)
            }
            Some(Ok(_)) => JsOutcome::Error("Rejected Promise returned a value".to_string()),
            None => JsOutcome::Error("Rejected Promise had no reason".to_string()),
        },
    }
}

fn drain_pending_jobs(
    rt: &Runtime,
    deadline: Instant,
    max_pending_jobs: usize,
    cancellation: &PermCancellation,
    permission_bridge: &PermissionBridge,
) -> Option<JsOutcome> {
    let mut executed = 0;

    loop {
        if let Some(outcome) = interruption_outcome(deadline, cancellation, permission_bridge) {
            return Some(outcome);
        }

        if executed >= max_pending_jobs {
            return rt.is_job_pending().then_some(JsOutcome::Timeout);
        }

        match rt.execute_pending_job() {
            Ok(true) => executed += 1,
            Ok(false) => return None,
            Err(job_exception) => {
                let outcome = job_exception.0.with(|ctx| {
                    let value = ctx.catch();
                    thrown_value_outcome(&ctx, value, deadline, cancellation, permission_bridge)
                });
                return Some(outcome);
            }
        }
    }
}

pub(crate) fn js_thread_main(
    rx: mpsc::Receiver<JsRequest>,
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
    allow_config: AllowConfig,
    execution_hosts: NormalExecutionHosts,
) {
    while let Ok(req) = rx.recv() {
        if req.reply.is_closed() {
            req.cancellation.cancel();
            log_reply_drop(ReplyPath::AbandonedBeforeExecution);
            continue;
        }
        if req.cancellation.is_cancelled() {
            deliver_reply_or_cancel(
                req.reply,
                JsResponse {
                    outcome: JsOutcome::Error("execution cancelled".to_string()),
                    #[cfg(feature = "skills")]
                    skill_events: Vec::new(),
                    #[cfg(feature = "skills")]
                    evidence_complete: false,
                },
                &req.cancellation,
                ReplyPath::EarlyCancel,
            );
            continue;
        }
        let bridge = permission_bridge.for_invocation(req.cancellation.clone());
        #[cfg(feature = "skills")]
        let response = run_step_with_skills(
            &req.code,
            &req.skill_bundle,
            &req.skill_tool_call_id,
            &sandbox,
            &bridge,
            &req.cancellation,
            &runtime,
            &allow_config,
            &execution_hosts,
        );
        #[cfg(not(feature = "skills"))]
        let response = JsResponse {
            outcome: run_step(
                &req.code,
                &sandbox,
                &bridge,
                &req.cancellation,
                &runtime,
                &allow_config,
                &execution_hosts,
            ),
        };
        deliver_reply_or_cancel(req.reply, response, &req.cancellation, ReplyPath::Completed);
    }
}

// pub(crate) required: Phase 3's verify_skill() calls this cross-module
#[cfg_attr(feature = "skills", allow(dead_code))]
pub(crate) fn run_step(
    code: &str,
    sandbox: &Sandbox,
    permission_bridge: &PermissionBridge,
    cancellation: &PermCancellation,
    runtime: &tokio::runtime::Handle,
    allow_config: &AllowConfig,
    execution_hosts: &NormalExecutionHosts,
) -> JsOutcome {
    run_step_with_policy(
        code,
        sandbox,
        permission_bridge,
        cancellation,
        runtime,
        allow_config,
        execution_hosts,
        #[cfg(feature = "skills")]
        None,
        #[cfg(feature = "skills")]
        None,
        ExecutionPolicy {
            timeout: STEP_TIMEOUT,
            max_pending_jobs: MAX_PENDING_JOBS,
        },
    )
}

#[cfg(feature = "skills")]
#[allow(clippy::too_many_arguments)]
fn run_step_with_skills(
    code: &str,
    skills: &crate::extras::js::skills::turn::TurnSkillBundle,
    tool_call_id: &str,
    sandbox: &Sandbox,
    permission_bridge: &PermissionBridge,
    cancellation: &PermCancellation,
    runtime: &tokio::runtime::Handle,
    allow_config: &AllowConfig,
    execution_hosts: &NormalExecutionHosts,
) -> JsResponse {
    let execution = SkillExecutionBundle::from_turn_bundle(skills, tool_call_id.to_string());
    let event_state = Arc::new(Mutex::new(InstrumentedEventState::default()));
    let outcome = run_step_with_policy(
        code,
        sandbox,
        permission_bridge,
        cancellation,
        runtime,
        allow_config,
        execution_hosts,
        Some(&execution),
        Some(&event_state),
        ExecutionPolicy {
            timeout: STEP_TIMEOUT,
            max_pending_jobs: MAX_PENDING_JOBS,
        },
    );
    let mut state = event_state
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    state.finalize_pending(&outcome);
    let evidence_complete =
        !matches!(outcome, JsOutcome::Timeout | JsOutcome::OomKilled) || state.pending.is_empty();
    JsResponse {
        outcome,
        skill_events: std::mem::take(&mut state.events),
        evidence_complete,
    }
}

#[cfg(feature = "skills")]
#[derive(Clone)]
struct EventMetadata {
    skill_id: String,
    export_name: Option<String>,
    turn_id: String,
    tool_call_id: String,
    retrieval_score: f64,
    retrieval_rank: u32,
    query_fingerprint: Option<String>,
    index_generation: u64,
    production: bool,
}

#[cfg(feature = "skills")]
#[derive(Default)]
struct InstrumentedEventState {
    events: Vec<crate::extras::js::skills::telemetry::SkillEvent>,
    starts: BTreeMap<String, Instant>,
    pending: BTreeMap<String, EventMetadata>,
}

#[cfg(feature = "skills")]
impl InstrumentedEventState {
    fn start(&mut self, invocation_id: String, metadata: EventMetadata, argument_shape: String) {
        self.starts.insert(invocation_id.clone(), Instant::now());
        self.pending.insert(invocation_id.clone(), metadata.clone());
        self.events.push(instrumented_event(
            &metadata,
            Some(invocation_id),
            crate::extras::js::skills::telemetry::SkillEventKind::Invoked,
            None,
            None,
            Some(argument_shape),
        ));
    }

    fn terminal(&mut self, invocation_id: &str, success: bool, capability_denied: bool) {
        let Some(metadata) = self.pending.remove(invocation_id) else {
            return;
        };
        let latency = self
            .starts
            .remove(invocation_id)
            .map(|start| start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
        let (kind, outcome) = if success {
            (
                crate::extras::js::skills::telemetry::SkillEventKind::Returned,
                "fulfilled",
            )
        } else if capability_denied {
            (
                crate::extras::js::skills::telemetry::SkillEventKind::CapabilityDenied,
                "capability_policy",
            )
        } else {
            (
                crate::extras::js::skills::telemetry::SkillEventKind::Threw,
                "exception",
            )
        };
        self.events.push(instrumented_event(
            &metadata,
            Some(invocation_id.to_string()),
            kind,
            Some(outcome.to_string()),
            latency,
            None,
        ));
    }

    fn finalize_pending(&mut self, outcome: &JsOutcome) {
        let pending = self.pending.keys().cloned().collect::<Vec<_>>();
        for invocation_id in pending {
            let Some(metadata) = self.pending.remove(&invocation_id) else {
                continue;
            };
            let latency = self
                .starts
                .remove(&invocation_id)
                .map(|start| start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
            let (kind, code) = match outcome {
                JsOutcome::Timeout => (
                    crate::extras::js::skills::telemetry::SkillEventKind::TimedOut,
                    "step_timeout",
                ),
                JsOutcome::OomKilled => (
                    crate::extras::js::skills::telemetry::SkillEventKind::Oom,
                    "step_oom",
                ),
                _ => (
                    crate::extras::js::skills::telemetry::SkillEventKind::Threw,
                    "step_cancelled_or_failed",
                ),
            };
            self.events.push(instrumented_event(
                &metadata,
                Some(invocation_id),
                kind,
                Some(code.to_string()),
                latency,
                None,
            ));
        }
        let step_succeeded = matches!(outcome, JsOutcome::Value(_) | JsOutcome::Void);
        for event in &mut self.events {
            if event.kind == crate::extras::js::skills::telemetry::SkillEventKind::Returned {
                event.outcome = Some(
                    if step_succeeded {
                        "fulfilled_step_succeeded"
                    } else {
                        "fulfilled_step_failed"
                    }
                    .to_string(),
                );
            }
        }
    }
}

#[cfg(feature = "skills")]
fn instrumented_event(
    metadata: &EventMetadata,
    invocation_id: Option<String>,
    kind: crate::extras::js::skills::telemetry::SkillEventKind,
    outcome: Option<String>,
    latency_us: Option<u64>,
    argument_shape: Option<String>,
) -> crate::extras::js::skills::telemetry::SkillEvent {
    crate::extras::js::skills::telemetry::SkillEvent {
        invocation_id,
        skill_id: metadata.skill_id.clone(),
        turn_id: metadata.turn_id.clone(),
        tool_call_id: Some(metadata.tool_call_id.clone()),
        kind,
        export_name: metadata.export_name.clone(),
        outcome,
        latency_us,
        retrieval_score: Some(metadata.retrieval_score),
        retrieval_rank: Some(metadata.retrieval_rank),
        query_fingerprint: metadata.query_fingerprint.clone(),
        index_generation: metadata.index_generation,
        evidence_complete: true,
        production: metadata.production,
        argument_shape,
        created_at: unix_timestamp(),
    }
}

#[cfg(feature = "skills")]
fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

#[cfg(feature = "skills")]
fn install_selected_skills(
    runtime: &Runtime,
    context: &Context,
    bundle: &SkillExecutionBundle,
    gate: &SkillCapabilityGate,
    state: &Arc<Mutex<InstrumentedEventState>>,
    deadline: Instant,
    cancellation: &PermCancellation,
    permission_bridge: &PermissionBridge,
) -> Result<(), JsOutcome> {
    if bundle.turn_id.is_empty()
        || bundle.turn_id.len() > crate::extras::js::skills::telemetry::MAX_EVENT_ID_BYTES
        || bundle.tool_call_id.is_empty()
        || bundle.tool_call_id.len() > crate::extras::js::skills::telemetry::MAX_EVENT_ID_BYTES
        || bundle.skills.len() > 64
        || bundle
            .skills
            .iter()
            .any(|skill| !skill.retrieval_score.is_finite())
    {
        return Err(JsOutcome::Error(
            "invalid bounded skill execution metadata".to_string(),
        ));
    }
    let mut declared = BTreeSet::new();
    let wrapper_factory = context.with(|ctx| {
        ctx.eval::<Value, _>(
            crate::extras::js::skills::SKILL_REALM_HARDENING_JS,
        )
        .map_err(|error| {
            skill_error_outcome(
                &ctx,
                "skill-realm-hardening",
                error,
                deadline,
                cancellation,
                permission_bridge,
            )
        })?;
        ctx.eval::<Value, _>(
            "(function(){\
             for (const name of ['read_file','write_file','fetch','spawn','console']) {\
               if (Object.prototype.hasOwnProperty.call(globalThis, name)) {\
                 Object.defineProperty(globalThis, name, {writable:false,configurable:false});\
               }\
             }\
             if (typeof console === 'object') Object.freeze(console);\
             })()",
        )
        .map_err(|error| {
            skill_error_outcome(
                &ctx,
                "capability-wrapper",
                error,
                deadline,
                cancellation,
                permission_bridge,
            )
        })?;
        let enter_gate = gate.clone();
        let enter = Function::new(ctx.clone(), move |skill_id: String| {
            enter_gate.push_registered(&skill_id)
        })
        .map_err(|error| {
            skill_error_outcome(
                &ctx,
                "capability-wrapper",
                error,
                deadline,
                cancellation,
                permission_bridge,
            )
        })?;
        let exit_gate = gate.clone();
        let exit =
            Function::new(ctx.clone(), move || exit_gate.pop_registered()).map_err(|error| {
                skill_error_outcome(
                    &ctx,
                    "capability-wrapper",
                    error,
                    deadline,
                    cancellation,
                    permission_bridge,
                )
            })?;
        let build_wrapper: Function = ctx
            .eval(
                "(enter, exit) => {\n\
                 const clone = (value) => {\n\
                   if (value === null || ['string','number','boolean','undefined'].includes(typeof value)) return value;\n\
                   if (typeof value === 'object') {\n\
                     if (typeof value.then === 'function') return value;\n\
                     const encoded = JSON.stringify(value);\n\
                     if (encoded === undefined) throw new TypeError('skill values must be JSON-safe');\n\
                     return JSON.parse(encoded);\n\
                   }\n\
                   throw new TypeError('skill values must not contain executable references');\n\
                 };\n\
                 return (fn, id) => function(...args) {\n\
                 enter(id);\n\
                 try {\n\
                   const value = fn.apply(undefined, args.map(clone));\n\
                   if (value && typeof value.then === 'function') {\n\
                     return Promise.resolve(value).then(clone);\n\
                   }\n\
                   return clone(value);\n\
                 } finally { exit(); }\n\
                 };\n\
                 }",
            )
            .map_err(|error| {
                skill_error_outcome(
                    &ctx,
                    "capability-wrapper",
                    error,
                    deadline,
                    cancellation,
                    permission_bridge,
                )
            })?;
        let wrapper_factory: Function = build_wrapper.call((enter, exit)).map_err(|error| {
            skill_error_outcome(
                &ctx,
                "capability-wrapper",
                error,
                deadline,
                cancellation,
                permission_bridge,
            )
        })?;
        Ok::<_, JsOutcome>(Persistent::save(&ctx, wrapper_factory))
    })?;
    if let Some(outcome) = drain_pending_jobs(
        runtime,
        deadline,
        MAX_PENDING_JOBS,
        cancellation,
        permission_bridge,
    ) {
        return Err(outcome);
    }

    for skill in &bundle.skills {
        let artifact = skill.artifact.clone();
        if let Err(error) = artifact.verify_identity() {
            return Err(JsOutcome::Error(format!(
                "selected skill {} failed identity validation: {error}",
                artifact.id
            )));
        }
        gate.register(artifact.id.clone(), artifact.capability.clone());
        let selected_metadata = EventMetadata {
            skill_id: artifact.id.clone(),
            export_name: None,
            turn_id: bundle.turn_id.clone(),
            tool_call_id: bundle.tool_call_id.clone(),
            retrieval_score: skill.retrieval_score,
            retrieval_rank: skill.retrieval_rank,
            query_fingerprint: skill.query_fingerprint.clone(),
            index_generation: bundle.index_generation,
            production: bundle.production,
        };
        let mut event_guard = state
            .lock()
            .map_err(|_| JsOutcome::Error("skill event state unavailable".to_string()))?;
        event_guard.events.push(instrumented_event(
            &selected_metadata,
            None,
            crate::extras::js::skills::telemetry::SkillEventKind::Selected,
            None,
            None,
            None,
        ));
        event_guard.events.push(instrumented_event(
            &selected_metadata,
            None,
            crate::extras::js::skills::telemetry::SkillEventKind::Injected,
            None,
            None,
            None,
        ));
        drop(event_guard);
        context.with(|ctx| {
            let globals = ctx.globals();
            for export in &artifact.exports {
                if !declared.insert(export.name.clone()) {
                    return Err(JsOutcome::Error(format!(
                        "selected skill {} declares duplicate export {}",
                        artifact.id, export.name
                    )));
                }
                match globals.contains_key(export.name.as_str()) {
                    Ok(true) => {
                        return Err(JsOutcome::Error(format!(
                            "selected skill {} export {} collides with an existing global",
                            artifact.id, export.name
                        )));
                    }
                    Ok(false) => {}
                    Err(error) => {
                        return Err(skill_error_outcome(
                            &ctx,
                            &artifact.id,
                            error,
                            deadline,
                            cancellation,
                            permission_bridge,
                        ));
                    }
                }
            }
            Ok::<_, JsOutcome>(())
        })?;

        let private_source = crate::extras::js::skills::private_skill_source(&artifact);
        let namespace = {
            let _active = gate.enter(artifact.capability.clone());
            let namespace = context.with(|ctx| {
                let mut options = EvalOptions::default();
                options.filename = Some(format!("skill-{}.js", artifact.id));
                ctx.eval_with_options::<Object, _>(private_source.as_bytes(), options)
                    .map(|namespace| Persistent::save(&ctx, namespace))
                    .map_err(|error| {
                        skill_error_outcome(
                            &ctx,
                            &artifact.id,
                            error,
                            deadline,
                            cancellation,
                            permission_bridge,
                        )
                    })
            })?;
            if let Some(outcome) = drain_pending_jobs(
                runtime,
                deadline,
                MAX_PENDING_JOBS,
                cancellation,
                permission_bridge,
            ) {
                return Err(outcome);
            }
            namespace
        };

        context.with(|ctx| {
            let globals = ctx.globals();
            let namespace = namespace.restore(&ctx).map_err(|error| {
                skill_error_outcome(
                    &ctx,
                    &artifact.id,
                    error,
                    deadline,
                    cancellation,
                    permission_bridge,
                )
            })?;
            let wrapper_factory = wrapper_factory.clone().restore(&ctx).map_err(|error| {
                skill_error_outcome(
                    &ctx,
                    &artifact.id,
                    error,
                    deadline,
                    cancellation,
                    permission_bridge,
                )
            })?;
            for export in &artifact.exports {
                let original: Function = namespace.get(export.name.as_str()).map_err(|error| {
                    skill_error_outcome(
                        &ctx,
                        &artifact.id,
                        error,
                        deadline,
                        cancellation,
                        permission_bridge,
                    )
                })?;
                let safe_wrapper: Function = wrapper_factory
                    .call((original, artifact.id.clone()))
                    .map_err(|error| {
                        skill_error_outcome(
                            &ctx,
                            &artifact.id,
                            error,
                            deadline,
                            cancellation,
                            permission_bridge,
                        )
                    })?;
                let metadata = EventMetadata {
                    skill_id: artifact.id.clone(),
                    export_name: Some(export.name.clone()),
                    turn_id: bundle.turn_id.clone(),
                    tool_call_id: bundle.tool_call_id.clone(),
                    retrieval_score: skill.retrieval_score,
                    retrieval_rank: skill.retrieval_rank,
                    query_fingerprint: skill.query_fingerprint.clone(),
                    index_generation: bundle.index_generation,
                    production: bundle.production,
                };
                let attribution =
                    crate::extras::js::skills::capability::SkillExecutionAttribution {
                        skill_id: artifact.id.clone(),
                        export_name: export.name.clone(),
                        manifest: artifact.capability.clone(),
                    };
                let ordinal = Arc::new(AtomicU32::new(0));
                let start_ordinal = Arc::clone(&ordinal);
                let start_state = Arc::clone(state);
                let start_metadata = metadata.clone();
                let invocation_context = gate.context();
                let invocation_attribution = attribution.clone();
                let on_start = Function::new(ctx.clone(), move |argument_shape: String| {
                    let ordinal = start_ordinal
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                            value.checked_add(1)
                        })
                        .map_err(|_| {
                            Error::new_from_js_message(
                                "skill invocation",
                                "instrumentation",
                                "invocation ordinal exhausted",
                            )
                        })?;
                    let invocation_id = crate::extras::js::skills::telemetry::stable_invocation_id(
                        &start_metadata.turn_id,
                        &start_metadata.tool_call_id,
                        &start_metadata.skill_id,
                        start_metadata.export_name.as_deref().unwrap_or(""),
                        ordinal,
                    );
                    let bounded_shape = if argument_shape.len()
                        <= crate::extras::js::skills::telemetry::MAX_ARGUMENT_SHAPE_BYTES
                    {
                        argument_shape
                    } else {
                        r#"{"truncated":true}"#.to_string()
                    };
                    start_state
                        .lock()
                        .map_err(|_| {
                            Error::new_from_js_message(
                                "skill invocation",
                                "instrumentation",
                                "event state unavailable",
                            )
                        })?
                        .start(invocation_id.clone(), start_metadata.clone(), bounded_shape);
                    invocation_context
                        .begin_invocation(invocation_id.clone(), invocation_attribution.clone())
                        .map_err(|error| {
                            Error::new_from_js_message(
                                "skill capability policy",
                                "invocation",
                                error.to_string(),
                            )
                        })?;
                    Ok::<String, Error>(invocation_id)
                })
                .map_err(|error| {
                    skill_error_outcome(
                        &ctx,
                        &artifact.id,
                        error,
                        deadline,
                        cancellation,
                        permission_bridge,
                    )
                })?;
                let terminal_state = Arc::clone(state);
                let terminal_context = gate.context();
                let on_terminal = Function::new(
                    ctx.clone(),
                    move |invocation_id: String, success: bool, capability_denied: bool| {
                        terminal_state
                            .lock()
                            .map_err(|_| {
                                Error::new_from_js_message(
                                    "skill invocation",
                                    "instrumentation",
                                    "event state unavailable",
                                )
                            })?
                            .terminal(&invocation_id, success, capability_denied);
                        terminal_context
                            .end_invocation(&invocation_id)
                            .map_err(|error| {
                                Error::new_from_js_message(
                                    "skill capability policy",
                                    "invocation",
                                    error.to_string(),
                                )
                            })?;
                        Ok::<(), Error>(())
                    },
                )
                .map_err(|error| {
                    skill_error_outcome(
                        &ctx,
                        &artifact.id,
                        error,
                        deadline,
                        cancellation,
                        permission_bridge,
                    )
                })?;
                let telemetry_factory: Function = ctx
                    .eval(
                        r#"(function(original, onStart, onTerminal, undeclaredHostReference) {
                            return function(...args) {
                                const shape = JSON.stringify({
                                    argc: args.length,
                                    types: args.map((value) =>
                                        value === null ? "null" :
                                        Array.isArray(value) ? "array" : typeof value
                                    )
                                });
                                const invocationId = onStart(shape);
                                try {
                                    const result = Reflect.apply(original, this, args);
                                    if (result && typeof result.then === "function") {
                                        return Promise.resolve(result).then(
                                            (value) => {
                                                onTerminal(invocationId, true, false);
                                                return value;
                                            },
                                            (error) => {
                                                const text = String(error);
                                                const denied =
                                                    text.includes("skill capability policy") ||
                                                    text.includes("skill capability") ||
                                                    undeclaredHostReference ||
                                                    ["read_file", "write_file", "spawn", "fetch"]
                                                        .some((host) => text.includes(host));
                                                onTerminal(invocationId, false, denied);
                                                throw error;
                                            }
                                        );
                                    }
                                    onTerminal(invocationId, true, false);
                                    return result;
                                } catch (error) {
                                    const text = String(error);
                                    const denied =
                                        text.includes("skill capability policy") ||
                                        text.includes("skill capability") ||
                                        undeclaredHostReference ||
                                        ["read_file", "write_file", "spawn", "fetch"]
                                            .some((host) => text.includes(host));
                                    onTerminal(invocationId, false, denied);
                                    throw error;
                                }
                            };
                        })"#,
                    )
                    .map_err(|error| {
                        skill_error_outcome(
                            &ctx,
                            &artifact.id,
                            error,
                            deadline,
                            cancellation,
                            permission_bridge,
                        )
                    })?;
                let wrapper: Function = telemetry_factory
                    .call((
                        safe_wrapper,
                        on_start,
                        on_terminal,
                        [
                            (
                                crate::extras::js::skills::HostCapability::ReadFile,
                                "read_file",
                            ),
                            (
                                crate::extras::js::skills::HostCapability::WriteFile,
                                "write_file",
                            ),
                            (crate::extras::js::skills::HostCapability::Spawn, "spawn"),
                            (crate::extras::js::skills::HostCapability::Fetch, "fetch"),
                        ]
                        .into_iter()
                        .any(|(capability, name)| {
                            !artifact.capability.allows(capability)
                                && artifact.source.contains(name)
                        }),
                    ))
                    .map_err(|error| {
                        skill_error_outcome(
                            &ctx,
                            &artifact.id,
                            error,
                            deadline,
                            cancellation,
                            permission_bridge,
                        )
                    })?;
                globals
                    .set(export.name.as_str(), wrapper)
                    .map_err(|error| {
                        skill_error_outcome(
                            &ctx,
                            &artifact.id,
                            error,
                            deadline,
                            cancellation,
                            permission_bridge,
                        )
                    })?;
            }
            Ok::<_, JsOutcome>(())
        })?;
    }
    Ok(())
}

#[cfg(feature = "skills")]
fn skill_error_outcome(
    ctx: &Ctx<'_>,
    skill_id: &str,
    error: Error,
    deadline: Instant,
    cancellation: &PermCancellation,
    permission_bridge: &PermissionBridge,
) -> JsOutcome {
    match error_outcome(ctx, error, deadline, cancellation, permission_bridge) {
        JsOutcome::Error(error) => {
            JsOutcome::Error(format!("selected skill {skill_id} failed: {error}"))
        }
        outcome => outcome,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_step_with_policy(
    code: &str,
    sandbox: &Sandbox,
    permission_bridge: &PermissionBridge,
    cancellation: &PermCancellation,
    runtime: &tokio::runtime::Handle,
    allow_config: &AllowConfig,
    execution_hosts: &NormalExecutionHosts,
    #[cfg(feature = "skills")] skills: Option<&SkillExecutionBundle>,
    #[cfg(feature = "skills")] event_state: Option<&Arc<Mutex<InstrumentedEventState>>>,
    policy: ExecutionPolicy,
) -> JsOutcome {
    #[cfg(not(feature = "skills"))]
    let _ = execution_hosts;

    // Fresh Runtime EVERY step — OOM poisons allocator; never reuse
    let rt = match Runtime::new() {
        Ok(r) => r,
        Err(e) => return JsOutcome::Error(format!("Runtime::new failed: {e}")),
    };
    rt.set_memory_limit(MEMORY_LIMIT);
    rt.set_max_stack_size(STACK_LIMIT);

    let deadline = Instant::now() + policy.timeout;
    let interrupt_cancellation = cancellation.clone();
    let interrupt_bridge = permission_bridge.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        Instant::now() >= deadline
            || interrupt_cancellation.is_cancelled()
            || interrupt_bridge.is_shutdown()
    })));

    let ctx = match Context::full(&rt) {
        Ok(c) => c,
        Err(Error::Allocation) => return JsOutcome::OomKilled,
        Err(e) => return JsOutcome::Error(format!("Context::full failed: {e}")),
    };

    #[cfg(feature = "skills")]
    let skill_gate = SkillCapabilityGate::default();
    if let Err(error) = register_host_globals(
        &ctx,
        sandbox.clone(),
        permission_bridge.clone(),
        runtime.clone(),
        allow_config.clone(),
        #[cfg(feature = "skills")]
        skill_gate.clone(),
    ) {
        return match error {
            Error::Allocation => JsOutcome::OomKilled,
            error => JsOutcome::Error(format!("Failed to register host globals: {error}")),
        };
    }
    #[cfg(feature = "skills")]
    if let Err(error) = register_proposal_global(&ctx, execution_hosts.proposal.clone()) {
        return match error {
            Error::Allocation => JsOutcome::OomKilled,
            error => JsOutcome::Error(format!("Failed to register proposal global: {error}")),
        };
    }

    #[cfg(feature = "skills")]
    if let (Some(skills), Some(event_state)) = (skills, event_state)
        && let Err(outcome) = install_selected_skills(
            &rt,
            &ctx,
            skills,
            &skill_gate,
            event_state,
            deadline,
            cancellation,
            permission_bridge,
        )
    {
        return outcome;
    }

    let evaluated: Result<Persistent<Value<'static>>, JsOutcome> = ctx.with(|ctx| {
        let mut options = EvalOptions::default();
        options.filename = Some("agent.js".to_string());
        ctx.eval_with_options::<Value, _>(code.as_bytes(), options)
            .map(|value| Persistent::save(&ctx, value))
            .map_err(|error| error_outcome(&ctx, error, deadline, cancellation, permission_bridge))
    });

    // rquickjs executes one Promise job per call. Use the eval deadline for the
    // whole queue and cap turns so a self-replenishing microtask chain cannot
    // monopolize the dedicated JS thread before the wall-clock deadline.
    let job_outcome = drain_pending_jobs(
        &rt,
        deadline,
        policy.max_pending_jobs,
        cancellation,
        permission_bridge,
    );

    match evaluated {
        Err(JsOutcome::Timeout) => JsOutcome::Timeout,
        Err(JsOutcome::OomKilled) => JsOutcome::OomKilled,
        Err(error) => match job_outcome {
            Some(JsOutcome::Timeout) => JsOutcome::Timeout,
            Some(JsOutcome::OomKilled) => JsOutcome::OomKilled,
            _ => error,
        },
        Ok(value) => match job_outcome {
            Some(outcome) => outcome,
            None => ctx.with(|ctx| match value.restore(&ctx) {
                Ok(value) => {
                    settled_value_outcome(&ctx, value, deadline, cancellation, permission_bridge)
                }
                Err(Error::Allocation) => JsOutcome::OomKilled,
                Err(error) => JsOutcome::Error(format!("Failed to restore JS result: {error}")),
            }),
        },
    }
    // rt drops here — RAII; Context must be dropped before Runtime
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_step_for_test(
    code: &str,
    sandbox: &Sandbox,
    permission_bridge: &PermissionBridge,
    cancellation: &PermCancellation,
    runtime: &tokio::runtime::Handle,
    allow_config: &AllowConfig,
    timeout: Duration,
    max_pending_jobs: usize,
) -> JsOutcome {
    run_step_with_policy(
        code,
        sandbox,
        permission_bridge,
        cancellation,
        runtime,
        allow_config,
        &NormalExecutionHosts::default(),
        #[cfg(feature = "skills")]
        None,
        #[cfg(feature = "skills")]
        None,
        ExecutionPolicy {
            timeout,
            max_pending_jobs,
        },
    )
}

#[cfg(all(test, feature = "skills"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_instrumented_step_for_test(
    code: &str,
    bundle: &SkillExecutionBundle,
    sandbox: &Sandbox,
    permission_bridge: &PermissionBridge,
    cancellation: &PermCancellation,
    runtime: &tokio::runtime::Handle,
    allow_config: &AllowConfig,
) -> JsResponse {
    let event_state = Arc::new(Mutex::new(InstrumentedEventState::default()));
    let outcome = run_step_with_policy(
        code,
        sandbox,
        permission_bridge,
        cancellation,
        runtime,
        allow_config,
        &NormalExecutionHosts::default(),
        Some(bundle),
        Some(&event_state),
        ExecutionPolicy {
            timeout: STEP_TIMEOUT,
            max_pending_jobs: MAX_PENDING_JOBS,
        },
    );
    let mut state = event_state
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    state.finalize_pending(&outcome);
    JsResponse {
        evidence_complete: !matches!(outcome, JsOutcome::Timeout | JsOutcome::OomKilled)
            || state.pending.is_empty(),
        outcome,
        skill_events: std::mem::take(&mut state.events),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::js::tool::PermissionBridgeOwner;

    #[tokio::test]
    async fn js_reply_delivery_cancels_dropped_receivers_and_recovers() {
        struct NoFormattingTraits;

        assert_eq!(ReplyPath::EarlyCancel.as_str(), "early_cancel");
        assert_eq!(
            ReplyPath::AbandonedBeforeExecution.as_str(),
            "abandoned_before_execution"
        );
        assert_eq!(ReplyPath::Completed.as_str(), "completed");

        let delivered_cancellation = PermCancellation::new();
        let (delivered_reply, delivered_receiver) = oneshot::channel();
        deliver_reply_or_cancel(
            delivered_reply,
            JsResponse {
                outcome: JsOutcome::Value("delivered".to_string()),
                #[cfg(feature = "skills")]
                skill_events: Vec::new(),
                #[cfg(feature = "skills")]
                evidence_complete: true,
            },
            &delivered_cancellation,
            ReplyPath::Completed,
        );
        assert!(!delivered_cancellation.is_cancelled());
        assert_eq!(
            delivered_receiver
                .await
                .expect("delivered reply channel should remain open")
                .outcome,
            JsOutcome::Value("delivered".to_string())
        );

        let cancelled_cancellation = PermCancellation::new();
        let (cancelled_reply, cancelled_receiver) = oneshot::channel::<NoFormattingTraits>();
        drop(cancelled_receiver);
        deliver_reply_or_cancel(
            cancelled_reply,
            NoFormattingTraits,
            &cancelled_cancellation,
            ReplyPath::EarlyCancel,
        );
        assert!(cancelled_cancellation.is_cancelled());

        let completed_cancellation = PermCancellation::new();
        let (completed_reply, completed_receiver) = oneshot::channel::<NoFormattingTraits>();
        drop(completed_receiver);
        deliver_reply_or_cancel(
            completed_reply,
            NoFormattingTraits,
            &completed_cancellation,
            ReplyPath::Completed,
        );
        assert!(completed_cancellation.is_cancelled());

        let recovery_cancellation = PermCancellation::new();
        let (recovery_reply, recovery_receiver) = oneshot::channel();
        deliver_reply_or_cancel(
            recovery_reply,
            JsResponse {
                outcome: JsOutcome::Value("recovered".to_string()),
                #[cfg(feature = "skills")]
                skill_events: Vec::new(),
                #[cfg(feature = "skills")]
                evidence_complete: true,
            },
            &recovery_cancellation,
            ReplyPath::Completed,
        );
        assert!(!recovery_cancellation.is_cancelled());
        assert_eq!(
            recovery_receiver
                .await
                .expect("reply delivery should recover after dropped receivers")
                .outcome,
            JsOutcome::Value("recovered".to_string())
        );
    }

    #[tokio::test]
    async fn js_reply_receiver_drop_cancels_abandoned_request_and_thread_recovers() {
        let permission_owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let permission_bridge = permission_owner.bridge();
        let (request_tx, request_rx) = mpsc::channel();
        let runtime = tokio::runtime::Handle::current();
        let js_thread = std::thread::Builder::new()
            .name("js-engine-reply-drop-test".into())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                js_thread_main(
                    request_rx,
                    Sandbox::new(false, "bwrap"),
                    permission_bridge,
                    runtime,
                    AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
                    NormalExecutionHosts::default(),
                );
            })
            .expect("failed to spawn JS test thread");

        let abandoned_cancellation = PermCancellation::new();
        let abandoned_cancellation_observer = abandoned_cancellation.clone();
        let (abandoned_reply, abandoned_receiver) = oneshot::channel();
        drop(abandoned_receiver);
        request_tx
            .send(JsRequest {
                code: "abandoned request must not run".to_string(),
                #[cfg(feature = "skills")]
                skill_bundle: std::sync::Arc::new(
                    crate::extras::js::skills::turn::TurnSkillBundle::empty("test"),
                ),
                #[cfg(feature = "skills")]
                skill_tool_call_id: "abandoned-tool".to_string(),
                cancellation: abandoned_cancellation,
                reply: abandoned_reply,
            })
            .expect("abandoned request should reach JS thread");

        let (recovery_reply, recovery_receiver) = oneshot::channel();
        request_tx
            .send(JsRequest {
                code: "40 + 2".to_string(),
                #[cfg(feature = "skills")]
                skill_bundle: std::sync::Arc::new(
                    crate::extras::js::skills::turn::TurnSkillBundle::empty("test"),
                ),
                #[cfg(feature = "skills")]
                skill_tool_call_id: "recovery-tool".to_string(),
                cancellation: PermCancellation::new(),
                reply: recovery_reply,
            })
            .expect("recovery request should reach JS thread");

        let recovery = tokio::time::timeout(Duration::from_secs(5), recovery_receiver)
            .await
            .expect("JS thread stopped after a reply receiver was dropped")
            .expect("JS thread closed the recovery reply channel");
        assert_eq!(recovery.outcome, JsOutcome::Value("42".to_string()));
        assert!(
            abandoned_cancellation_observer.is_cancelled(),
            "JS thread should cancel an abandoned request before skipping it"
        );

        drop(request_tx);
        js_thread.join().expect("JS test thread panicked");
    }
}
