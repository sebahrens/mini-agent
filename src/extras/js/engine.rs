use rquickjs::promise::PromiseState;
use rquickjs::{Coerced, Context, Ctx, Error, FromJs, Persistent, Runtime, Value};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

use crate::extras::js::host::register_host_globals;
use crate::extras::js::tool::PermissionBridge;
use crate::extras::js::types::*;
use crate::sandbox::Sandbox;

const MAX_PENDING_JOBS: usize = 10_000;

fn send_reply_or_log_drop(
    reply: oneshot::Sender<JsResponse>,
    outcome: JsOutcome,
    reply_path: &'static str,
) {
    // The send error owns JsResponse; keep this diagnostic independent of its formatting traits.
    if reply.send(JsResponse { outcome }).is_err() {
        tracing::debug!(
            reply_path,
            "JS engine reply receiver dropped before response delivery"
        );
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
) {
    while let Ok(req) = rx.recv() {
        if req.cancellation.is_cancelled() {
            send_reply_or_log_drop(
                req.reply,
                JsOutcome::Error("execution cancelled".to_string()),
                "early_cancel",
            );
            continue;
        }
        let bridge = permission_bridge.for_invocation(req.cancellation.clone());
        let outcome = run_step(&req.code, &sandbox, &bridge, &req.cancellation, &runtime);
        send_reply_or_log_drop(req.reply, outcome, "completed");
    }
}

// pub(crate) required: Phase 3's verify_skill() calls this cross-module
pub(crate) fn run_step(
    code: &str,
    sandbox: &Sandbox,
    permission_bridge: &PermissionBridge,
    cancellation: &PermCancellation,
    runtime: &tokio::runtime::Handle,
) -> JsOutcome {
    run_step_with_policy(
        code,
        sandbox,
        permission_bridge,
        cancellation,
        runtime,
        ExecutionPolicy {
            timeout: STEP_TIMEOUT,
            max_pending_jobs: MAX_PENDING_JOBS,
        },
    )
}

fn run_step_with_policy(
    code: &str,
    sandbox: &Sandbox,
    permission_bridge: &PermissionBridge,
    cancellation: &PermCancellation,
    runtime: &tokio::runtime::Handle,
    policy: ExecutionPolicy,
) -> JsOutcome {
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

    if let Err(error) = register_host_globals(
        &ctx,
        sandbox.clone(),
        permission_bridge.clone(),
        runtime.clone(),
    ) {
        return match error {
            Error::Allocation => JsOutcome::OomKilled,
            error => JsOutcome::Error(format!("Failed to register host globals: {error}")),
        };
    }

    let evaluated: Result<Persistent<Value<'static>>, JsOutcome> = ctx.with(|ctx| {
        ctx.eval::<Value, _>(code)
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
pub(crate) fn run_step_for_test(
    code: &str,
    sandbox: &Sandbox,
    permission_bridge: &PermissionBridge,
    cancellation: &PermCancellation,
    runtime: &tokio::runtime::Handle,
    timeout: Duration,
    max_pending_jobs: usize,
) -> JsOutcome {
    run_step_with_policy(
        code,
        sandbox,
        permission_bridge,
        cancellation,
        runtime,
        ExecutionPolicy {
            timeout,
            max_pending_jobs,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::js::tool::PermissionBridgeOwner;

    #[tokio::test]
    async fn js_reply_receiver_drop_is_non_fatal_for_cancelled_and_completed_requests() {
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
                );
            })
            .expect("failed to spawn JS test thread");

        let cancellation = PermCancellation::new();
        cancellation.cancel();
        let (cancelled_reply, cancelled_receiver) = oneshot::channel();
        drop(cancelled_receiver);
        request_tx
            .send(JsRequest {
                code: "unreachable".to_string(),
                cancellation,
                reply: cancelled_reply,
            })
            .expect("cancelled request should reach JS thread");

        let (completed_reply, completed_receiver) = oneshot::channel();
        drop(completed_receiver);
        request_tx
            .send(JsRequest {
                code: "40 + 1".to_string(),
                cancellation: PermCancellation::new(),
                reply: completed_reply,
            })
            .expect("normal request should reach JS thread");

        let (recovery_reply, recovery_receiver) = oneshot::channel();
        request_tx
            .send(JsRequest {
                code: "40 + 2".to_string(),
                cancellation: PermCancellation::new(),
                reply: recovery_reply,
            })
            .expect("recovery request should reach JS thread");

        let recovery = tokio::time::timeout(Duration::from_secs(5), recovery_receiver)
            .await
            .expect("JS thread stopped after a reply receiver was dropped")
            .expect("JS thread closed the recovery reply channel");
        assert_eq!(recovery.outcome, JsOutcome::Value("42".to_string()));

        drop(request_tx);
        js_thread.join().expect("JS test thread panicked");
    }
}
