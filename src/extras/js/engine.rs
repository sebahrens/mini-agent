use rquickjs::{Context, Runtime, Value};
use std::sync::mpsc;
use std::time::Instant;

use crate::extras::js::host::register_host_globals;
use crate::extras::js::tool::PermissionBridge;
use crate::extras::js::types::*;
use crate::sandbox::Sandbox;

fn exception_details(
    exception: Option<&rquickjs::Exception<'_>>,
) -> Result<(String, String), JsOutcome> {
    let exception = exception
        .ok_or_else(|| JsOutcome::Error("Failed to extract exception".to_string()))?;
    Ok((
        exception.message().unwrap_or_default(),
        exception.stack().unwrap_or_default(),
    ))
}

pub(crate) fn js_thread_main(
    rx: mpsc::Receiver<JsRequest>,
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) {
    while let Ok(req) = rx.recv() {
        if req.cancellation.is_cancelled() {
            let _ = req.reply.send(JsResponse {
                outcome: JsOutcome::Error("execution cancelled".to_string()),
            });
            continue;
        }
        let bridge = permission_bridge.for_invocation(req.cancellation.clone());
        let outcome = run_step(&req.code, &sandbox, &bridge, &req.cancellation, &runtime);
        let _ = req.reply.send(JsResponse { outcome });
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
    // Fresh Runtime EVERY step — OOM poisons allocator; never reuse
    let rt = match Runtime::new() {
        Ok(r) => r,
        Err(e) => return JsOutcome::Error(format!("Runtime::new failed: {e}")),
    };
    rt.set_memory_limit(MEMORY_LIMIT);
    rt.set_max_stack_size(STACK_LIMIT);

    let deadline = Instant::now() + STEP_TIMEOUT;
    let interrupt_cancellation = cancellation.clone();
    let interrupt_bridge = permission_bridge.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        Instant::now() >= deadline
            || interrupt_cancellation.is_cancelled()
            || interrupt_bridge.is_shutdown()
    })));

    let ctx = match Context::full(&rt) {
        Ok(c) => c,
        Err(e) => return JsOutcome::Error(format!("Context::full failed: {e}")),
    };

    if let Err(error) = register_host_globals(
        &ctx,
        sandbox.clone(),
        permission_bridge.clone(),
        runtime.clone(),
    ) {
        return JsOutcome::Error(format!("Failed to register host globals: {error}"));
    }

    let result = ctx.with(|ctx| {
        let outcome = match ctx.eval::<Value, _>(code) {
            Err(rquickjs::Error::Exception) => {
                let exc = ctx.catch();
                let (msg, stack) = exception_details(exc.as_exception())?;
                if msg.contains("interrupted") || Instant::now() >= deadline {
                    JsOutcome::Timeout
                } else {
                    JsOutcome::Error(format!("{msg}\n{stack}"))
                }
            }
            Err(e) => JsOutcome::Error(e.to_string()),
            Ok(v) => {
                if v.is_undefined() || v.is_null() {
                    JsOutcome::Void
                } else if let Some(s) = v.as_string() {
                    JsOutcome::Value(s.to_string().unwrap_or_default())
                } else if let Some(n) = v.as_int() {
                    JsOutcome::Value(n.to_string())
                } else if let Some(f) = v.as_float() {
                    JsOutcome::Value(f.to_string())
                } else if let Some(b) = v.as_bool() {
                    JsOutcome::Value(b.to_string())
                } else {
                    JsOutcome::Value(format!("{v:?}"))
                }
            }
        };
        Ok::<JsOutcome, JsOutcome>(outcome)
    });

    // Drain microtask queue — required for Promise resolution
    while matches!(rt.execute_pending_job(), Ok(true)) {}

    result.unwrap_or_else(|error| error)
    // rt drops here — RAII; Context must be dropped before Runtime
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_caught_exception_returns_error() {
        match exception_details(None) {
            Err(JsOutcome::Error(message)) => {
                assert_eq!(message, "Failed to extract exception");
            }
            outcome => panic!("unexpected outcome: {outcome:?}"),
        }
    }
}
