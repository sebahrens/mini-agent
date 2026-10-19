use std::collections::HashMap;
use std::sync::Arc;

use rig::tool::{ToolCallExtensions, ToolDyn, ToolError, ToolExecutionResult, ToolFailure};
use rig::wasm_compat::WasmBoxedFuture;

use crate::agent::tools::ToolError as LocalToolError;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

use super::dispatcher::HookDispatcher;
use super::normalize::McpToolIdentity;
use super::{Decision, HookCtx, Verdict, session_context};

static HOOK_PERMISSION_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The only rig-typed file in the hook system (see design D1/D2): wraps a
/// `ToolDyn` so `PreToolUse`/`PostToolUseFailure` run around the inner call.
/// Preserves string/extension/structured dispatch, including result metadata.
pub(crate) struct HookedTool {
    inner: Box<dyn ToolDyn>,
    dispatcher: Arc<HookDispatcher>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    mcp: Option<McpToolIdentity>,
}

/// Owns only this invocation's grant, including while its future is suspended.
struct HookPermissionGuard<'a> {
    permission: &'a PermCheck,
    token: u64,
}

impl Drop for HookPermissionGuard<'_> {
    fn drop(&mut self) {
        self.permission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear_hook_one_shot(self.token);
    }
}

impl HookedTool {
    fn build_ctx(&self) -> HookCtx {
        let (session_id, session_path) = session_context();
        let cwd = super::active_workspace().display().to_string();
        let permission_mode = self
            .permission
            .as_ref()
            .map(|p| {
                p.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .mode()
                    .to_string()
            })
            .unwrap_or_else(|| "standard".to_string());
        HookCtx {
            session_id,
            session_path,
            cwd,
            permission_mode,
        }
    }
    async fn run_with_hooks<T: Send + Sync, F>(
        &self,
        args: String,
        invoke: impl FnOnce(String) -> F + Send,
        inspect: fn(&T) -> (bool, &str),
        rewrite: fn(T, String) -> T,
    ) -> Result<T, ToolError>
    where
        F: std::future::Future<Output = Result<T, ToolError>> + Send,
    {
        let tool_name = self.inner.name();
        let ctx = self.build_ctx();
        let tool_input: serde_json::Value =
            serde_json::from_str(&args).unwrap_or(serde_json::Value::Null);

        let pre = self
            .dispatcher
            .dispatch_pre_tool_use_with_mcp(&ctx, &tool_name, tool_input.clone(), self.mcp.as_ref())
            .await;

        // A PreToolUse hook may rewrite the arguments the inner tool
        // actually runs with. Multiple rewrites are folded upstream in
        // declared order; this applies the folded result.
        let call_args = match &pre.updated_input {
            Some(rewritten) => serde_json::to_string(rewritten).unwrap_or(args),
            None => args,
        };
        // Post hooks audit the operation that actually ran, so they receive
        // the effective arguments after any pre-hook rewrite rather than
        // the arguments the model originally proposed.
        let executed_input: serde_json::Value = match &pre.updated_input {
            Some(rewritten) => rewritten.clone(),
            None => tool_input,
        };

        let permission_token =
            HOOK_PERMISSION_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut scoped_permission = false;

        match pre.verdict {
            Verdict::Deny => {
                let reason = pre.reason.unwrap_or_else(|| "denied by hook".to_string());
                if let Some(perm) = &self.permission {
                    perm.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .record_blocked(&tool_name, &call_args);
                }
                return Err(ToolError::ToolCallError(Box::new(LocalToolError::Msg(
                    format!("Blocked by guard rail: {reason}"),
                ))));
            }
            // An `ask` must be enforced before the tool runs. Wrapped
            // tools check operation-specific keys (`git/status`), shared
            // keys (`mcp_tool`) or nothing at all, so relying on an inner
            // check to consume a forced ask lets the verdict fall through
            // and the side effect happen. Prompt once here, or fail closed
            // when no approval channel exists.
            Verdict::Ask => {
                // MCP deny policy uses the raw server/tool key, never a
                // provider-sanitized model alias or rewritten arguments.
                if let (Some(mcp), Some(perm)) = (&self.mcp, &self.permission) {
                    let result = perm
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .hook_ask_decision("mcp_tool", &mcp.permission_input());
                    if let crate::permission::checker::CheckResult::Denied(reason) = result {
                        perm.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .record_blocked(&tool_name, &call_args);
                        return Err(ToolError::ToolCallError(Box::new(LocalToolError::Msg(
                            format!("Permission denied: {reason}"),
                        ))));
                    }
                }
                if let Err(error) = crate::agent::tools::request_hook_approval(
                    &self.permission,
                    &self.ask_tx,
                    &tool_name,
                    &call_args,
                )
                .await
                {
                    if let Some(perm) = &self.permission {
                        perm.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .record_blocked(&tool_name, &call_args);
                    }
                    return Err(ToolError::ToolCallError(Box::new(error)));
                }
                // The existing task-local token binds this approval to one
                // invocation, including inner operation-specific checks.
                if let Some(perm) = &self.permission {
                    perm.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .allow_once_scoped(tool_name.clone(), permission_token);
                    scoped_permission = true;
                }
            }
            // Suppresses the inner tool's own permission prompt for only
            // this call; never bypasses a deny rule (checked first in
            // `PermissionChecker::check`/`check_path`).
            Verdict::Allow => {
                if let Some(perm) = &self.permission {
                    perm.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .allow_once_scoped(tool_name.clone(), permission_token);
                    scoped_permission = true;
                }
            }
            Verdict::Defer => {}
        }

        let permission_guard =
            self.permission
                .as_ref()
                .filter(|_| scoped_permission)
                .map(|permission| HookPermissionGuard {
                    permission,
                    token: permission_token,
                });
        let inner = invoke(call_args);
        let result = if scoped_permission {
            crate::permission::checker::scope_hook_permission(permission_token, inner).await
        } else {
            inner.await
        };
        drop(permission_guard);

        match &result {
            Ok(response) if inspect(response).0 => {
                let output = inspect(response).1;
                let decision = if let Some(mcp) = &self.mcp {
                    self.dispatcher
                        .dispatch_post_tool_use_with_mcp(
                            &ctx,
                            &tool_name,
                            executed_input,
                            output,
                            Some(mcp),
                        )
                        .await
                } else {
                    self.dispatcher
                        .dispatch_post_tool_use(&ctx, &tool_name, executed_input, output)
                        .await
                };
                if let Decision::Rewrite { content } = decision {
                    return result.map(|original| rewrite(original, content));
                }
            }
            Ok(response) => {
                let output = inspect(response).1;
                self.dispatcher
                    .dispatch_post_tool_use_failure_with_mcp(
                        &ctx,
                        &tool_name,
                        executed_input,
                        output,
                        self.mcp.as_ref(),
                    )
                    .await;
            }
            Err(e) => {
                if let Some(mcp) = &self.mcp {
                    self.dispatcher
                        .dispatch_post_tool_use_failure_with_mcp(
                            &ctx,
                            &tool_name,
                            executed_input,
                            &e.to_string(),
                            Some(mcp),
                        )
                        .await;
                } else {
                    self.dispatcher
                        .dispatch_post_tool_use_failure(
                            &ctx,
                            &tool_name,
                            executed_input,
                            &e.to_string(),
                        )
                        .await;
                }
            }
        }

        result
    }
}

impl ToolDyn for HookedTool {
    fn name(&self) -> String {
        self.inner.name()
    }

    // `WasmBoxedFuture` is the return type rig's `ToolDyn` trait requires for
    // `call`. On native targets (this crate never builds for
    // wasm32) it is a plain `Pin<Box<dyn Future + Send>>`; rig only drops the
    // `Send` bound on wasm32.
    fn description(&self) -> String {
        self.inner.description()
    }

    fn parameters(&self) -> serde_json::Value {
        self.inner.parameters()
    }

    fn shared_definition(&self) -> Option<std::sync::Arc<rig::completion::ToolDefinition>> {
        self.inner.shared_definition()
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        // Select before constructing/polling the much larger hook pipeline.
        // Keep inner construction inside the returned future: dropping an
        // unpolled wrapper must not start even eager ToolDyn constructor work.
        if !self.dispatcher.has_tool_hooks() {
            return Box::pin(async move { self.inner.call(args).await });
        }
        crate::startup::heap_future(|| {
            self.run_with_hooks(
                args,
                |args| self.inner.call(args),
                |text| (true, text.as_str()),
                |_, rewritten| rewritten,
            )
        })
    }

    fn call_with_extensions<'a>(
        &'a self,
        args: String,
        extensions: &'a ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        if !self.dispatcher.has_tool_hooks() {
            return Box::pin(
                async move { self.inner.call_with_extensions(args, extensions).await },
            );
        }
        crate::startup::heap_future(|| {
            self.run_with_hooks(
                args,
                |args| self.inner.call_with_extensions(args, extensions),
                |text| (true, text.as_str()),
                |_, rewritten| rewritten,
            )
        })
    }

    fn call_structured<'a>(
        &'a self,
        args: String,
        extensions: &'a ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, ToolExecutionResult> {
        if !self.dispatcher.has_tool_hooks() {
            return Box::pin(async move { self.inner.call_structured(args, extensions).await });
        }
        crate::startup::heap_future(|| async move {
            match self
                .run_with_hooks(
                    args,
                    |args| async move { Ok(self.inner.call_structured(args, extensions).await) },
                    |result| (result.outcome().is_success(), result.model_output()),
                    |result, rewritten| {
                        ToolExecutionResult::success(rewritten)
                            .with_extensions(result.extensions().clone())
                    },
                )
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    // Match Rig's default manual-ToolDyn classification for
                    // pre-execution rejection. No inner result metadata exists.
                    let message = error.to_string();
                    let failure = match error {
                        ToolError::JsonError(_) => ToolFailure::invalid_args(message.clone()),
                        ToolError::ToolCallError(_) => ToolFailure::other(message.clone()),
                    };
                    ToolExecutionResult::failed(message, failure)
                }
            }
        })
    }
}

/// Wraps every tool with the hook dispatcher's guard rail. Returns `tools`
/// unchanged when the dispatcher has no configured hooks (zero-cost
/// invariant).
pub(crate) fn wrap_all(
    tools: Vec<Box<dyn ToolDyn>>,
    dispatcher: Arc<HookDispatcher>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
) -> Vec<Box<dyn ToolDyn>> {
    wrap_all_with_mcp(tools, dispatcher, permission, ask_tx, HashMap::new())
}

/// Registration-owned metadata follows the final model names through the
/// allowlist and definition memoizer; no global/name-inference registry exists.
pub(crate) fn wrap_all_with_mcp(
    tools: Vec<Box<dyn ToolDyn>>,
    dispatcher: Arc<HookDispatcher>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    mut mcp: HashMap<String, McpToolIdentity>,
) -> Vec<Box<dyn ToolDyn>> {
    if dispatcher.is_empty() {
        return tools;
    }
    tools
        .into_iter()
        .map(|inner| {
            let identity = mcp.remove(&inner.name());
            Box::new(HookedTool {
                inner,
                dispatcher: dispatcher.clone(),
                permission: permission.clone(),
                ask_tx: ask_tx.clone(),
                mcp: identity,
            }) as Box<dyn ToolDyn>
        })
        .collect()
}
