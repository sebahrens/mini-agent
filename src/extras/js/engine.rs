use rquickjs::{Context, Runtime, Value};
use std::sync::mpsc;
use std::time::Instant;

use crate::extras::js::host::register_host_globals;
use crate::extras::js::tool::PermissionBridge;
use crate::extras::js::types::*;
use crate::sandbox::Sandbox;

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

    register_host_globals(
        &ctx,
        sandbox.clone(),
        permission_bridge.clone(),
        runtime.clone(),
    );

    let result = ctx.with(|ctx| match ctx.eval::<Value, _>(code) {
        Err(rquickjs::Error::Exception) => {
            let exc = ctx.catch();
            let exc = exc.as_exception().expect("exception type");
            let msg = exc.message().unwrap_or_default();
            let stack = exc.stack().unwrap_or_default();
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
    });

    // Drain microtask queue — required for Promise resolution
    while matches!(rt.execute_pending_job(), Ok(true)) {}

    result
    // rt drops here — RAII; Context must be dropped before Runtime
}
