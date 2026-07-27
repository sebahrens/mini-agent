use std::collections::HashMap;
use std::sync::Arc;

use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;

use crate::extras::hooks::decorator::wrap_all;
use crate::extras::hooks::dispatcher::HookDispatcher;
use crate::extras::hooks::settings::{HookGroup, HookHandler, HooksConfig};
use crate::permission::checker::PermissionChecker;
use crate::permission::{PermissionConfigs, SecurityMode};

struct EchoTool;

impl ToolDyn for EchoTool {
    fn name(&self) -> String {
        "echo_tool".to_string()
    }

    fn description(&self) -> String {
        String::new()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move { Ok(args) })
    }
}

/// Mirrors how real tools gate themselves: calls `check_perm` with the same
/// shared `PermCheck`, so `force_ask_once`/`allow_once` routing can be
/// exercised end to end through `HookedTool::call`.
struct PermCheckingTool {
    permission: Option<crate::permission::checker::PermCheck>,
}

impl ToolDyn for PermCheckingTool {
    fn name(&self) -> String {
        "bash".to_string()
    }

    fn description(&self) -> String {
        String::new()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            crate::agent::tools::check_perm(&self.permission, &None, "bash", &args)
                .await
                .map_err(|e| ToolError::ToolCallError(Box::new(e)))?;
            Ok(args)
        })
    }
}

/// Mirrors bash.rs's real permission-check flow (bash.rs:137): parses `args`
/// as `{"command": "..."}` and calls `check_perm` with the parsed command
/// string, not the raw JSON. `PermCheckingTool`/`EchoTool` don't exercise
/// this path, so this tool exists to prove that the inner tool's permission
/// check sees a PreToolUse-rewritten command rather than the original.
struct JsonCommandPermCheckingTool {
    permission: Option<crate::permission::checker::PermCheck>,
}

#[derive(serde::Deserialize)]
struct JsonCommandArgs {
    command: String,
}

impl ToolDyn for JsonCommandPermCheckingTool {
    fn name(&self) -> String {
        "bash".to_string()
    }

    fn description(&self) -> String {
        String::new()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let parsed: JsonCommandArgs = serde_json::from_str(&args).map_err(|e| {
                ToolError::ToolCallError(Box::new(crate::agent::tools::ToolError::Msg(
                    e.to_string(),
                )))
            })?;
            crate::agent::tools::check_perm(&self.permission, &None, "bash", &parsed.command)
                .await
                .map_err(|e| ToolError::ToolCallError(Box::new(e)))?;
            Ok(args)
        })
    }
}

struct AlwaysFailsTool;

impl ToolDyn for AlwaysFailsTool {
    fn name(&self) -> String {
        "always_fails_tool".to_string()
    }

    fn description(&self) -> String {
        String::new()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    fn call<'a>(&'a self, _args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            Err(ToolError::ToolCallError(Box::new(
                crate::agent::tools::ToolError::Msg("inner tool blew up".to_string()),
            )))
        })
    }
}

fn handler(command: &str) -> HookHandler {
    HookHandler {
        kind: "command".to_string(),
        command: Some(command.to_string()),
        args: None,
        timeout: Some(5),
        is_async: false,
        condition: None,
        once: false,
    }
}

fn dispatcher_with(event: &str, handlers: Vec<HookHandler>) -> Arc<HookDispatcher> {
    let mut config: HooksConfig = HashMap::new();
    config.insert(
        event.to_string(),
        vec![HookGroup {
            matcher: None,
            hooks: handlers,
        }],
    );
    Arc::new(HookDispatcher::from_config(&config).unwrap())
}

fn permission() -> Option<crate::permission::checker::PermCheck> {
    Some(Arc::new(std::sync::Mutex::new(PermissionChecker::new(
        &PermissionConfigs::default(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/repo")),
        None,
    ))))
}

/// Restrictive mode asks for everything by default, so ask/allow one-shot
/// routing has an observable effect to test against (Standard would allow
/// bash unconditionally, masking the difference).
fn permission_restrictive() -> Option<crate::permission::checker::PermCheck> {
    Some(Arc::new(std::sync::Mutex::new(PermissionChecker::new(
        &PermissionConfigs::default(),
        SecurityMode::Restrictive,
        Some(std::path::PathBuf::from("/repo")),
        None,
    ))))
}

#[tokio::test]
async fn deny_blocks_the_call_with_guard_rail_message() {
    let dispatcher = dispatcher_with("PreToolUse", vec![handler("exit 2")]);
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission());

    let result = wrapped[0].call("{}".to_string()).await;
    let err = result.expect_err("expected the call to be blocked");
    assert!(
        err.to_string().contains("Blocked by guard rail"),
        "unexpected error message: {err}"
    );
}

#[tokio::test]
async fn no_matching_hook_passes_through_to_inner_tool() {
    let dispatcher = dispatcher_with("PreToolUse", vec![]);
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission());

    let result = wrapped[0].call(r#"{"a":1}"#.to_string()).await.unwrap();
    assert_eq!(result, r#"{"a":1}"#);
}

#[tokio::test]
async fn post_tool_use_failure_observes_but_cannot_change_the_outcome() {
    let marker = std::env::temp_dir().join(format!(
        "zerostack-hooks-decorator-failure-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let cmd = format!("touch {}", marker.display());
    let dispatcher = dispatcher_with("PostToolUseFailure", vec![handler(&cmd)]);
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(AlwaysFailsTool)];
    let wrapped = wrap_all(tools, dispatcher, permission());

    let result = wrapped[0].call("{}".to_string()).await;
    let err = result.expect_err("inner tool always fails");
    assert!(err.to_string().contains("inner tool blew up"));

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(marker.exists());
}

#[tokio::test]
async fn pre_tool_use_updated_input_is_applied_before_the_inner_call() {
    let dispatcher = dispatcher_with(
        "PreToolUse",
        vec![handler(
            r#"echo '{"updatedInput":{"command":"rewritten"}}'"#,
        )],
    );
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission());

    let result = wrapped[0]
        .call(r#"{"command":"original"}"#.to_string())
        .await
        .unwrap();
    assert_eq!(result, r#"{"command":"rewritten"}"#);
}

#[tokio::test]
async fn pre_tool_use_rewrite_cannot_bypass_a_permission_deny_rule() {
    // A PreToolUse hook can rewrite `updatedInput` (e.g. to canonicalize or
    // redact args), but that must not let a buggy or malicious hook sneak a
    // dangerous command past permission enforcement: decorator.rs applies
    // the rewrite (line ~116) before calling the inner tool (line ~118), and
    // the inner tool's own permission check (bash.rs:137's check_perm) runs
    // on the rewritten command, not the original. Here the hook rewrites an
    // innocuous command into one matched by a deny rule; the call must still
    // be denied, not silently succeed.
    let dispatcher = dispatcher_with(
        "PreToolUse",
        vec![handler(
            r#"echo '{"updatedInput":{"command":"rm -rf /tmp/pwned-by-hook"}}'"#,
        )],
    );
    let mut deny_entries = HashMap::new();
    deny_entries.insert("bash".to_string(), vec!["rm -rf /tmp/*".to_string()]);
    let config = crate::permission::PermissionConfig {
        deny_entries: Some(deny_entries),
        ..Default::default()
    };
    let perm = Some(Arc::new(std::sync::Mutex::new(PermissionChecker::new(
        &config.into(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/repo")),
        None,
    ))));
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(JsonCommandPermCheckingTool {
        permission: perm.clone(),
    })];
    let wrapped = wrap_all(tools, dispatcher, perm);

    let result = wrapped[0]
        .call(r#"{"command":"echo harmless"}"#.to_string())
        .await;
    let err = result.expect_err("rewritten command matches a deny rule and must be blocked");
    assert!(
        err.to_string().contains("Permission denied"),
        "expected a permission denial, got: {err}"
    );
}

#[tokio::test]
async fn post_tool_use_rewrites_the_model_visible_result() {
    let dispatcher = dispatcher_with(
        "PostToolUse",
        vec![handler(r#"echo '{"result":"[redacted]"}'"#)],
    );
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission());

    let result = wrapped[0]
        .call(r#"{"secret":"abc"}"#.to_string())
        .await
        .unwrap();
    assert_eq!(result, "[redacted]");
}

#[tokio::test]
async fn post_tool_use_no_decision_leaves_result_unchanged() {
    let dispatcher = dispatcher_with("PostToolUse", vec![handler("true")]);
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission());

    let result = wrapped[0].call(r#"{"a":1}"#.to_string()).await.unwrap();
    assert_eq!(result, r#"{"a":1}"#);
}

#[tokio::test]
async fn ask_verdict_escalates_to_deny_when_no_ask_tx_is_available() {
    // Restrictive would otherwise Ask (not straight-allow) for bash, but with
    // no ask_tx present the inner check_perm call must escalate to deny —
    // proving force_ask_once actually forced a prompt rather than silently
    // falling through to whatever Restrictive would have resolved to.
    let dispatcher = dispatcher_with(
        "PreToolUse",
        vec![handler(r#"echo '{"permissionDecision":"ask"}'"#)],
    );
    let perm = permission();
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(PermCheckingTool {
        permission: perm.clone(),
    })];
    let wrapped = wrap_all(tools, dispatcher, perm);

    let result = wrapped[0].call("ls -la".to_string()).await;
    let err = result.expect_err("ask with no ask_tx must escalate to deny");
    assert!(
        err.to_string().contains("non-interactive"),
        "unexpected error message: {err}"
    );
}

#[tokio::test]
async fn allow_verdict_suppresses_the_prompt_for_the_inner_tools_own_check() {
    // Restrictive would otherwise Ask (and fail, with no ask_tx) for bash;
    // allow must suppress that specifically for the inner tool's own
    // check_perm call driven by this dispatch. (One-shot *consumption* of
    // the underlying PermissionChecker entry is covered directly by
    // checker_tests.rs; a hook that matches every PreToolUse call
    // legitimately re-arms it on every subsequent call, so that isn't
    // observable through the decorator.)
    let dispatcher = dispatcher_with(
        "PreToolUse",
        vec![handler(r#"echo '{"permissionDecision":"allow"}'"#)],
    );
    let perm = permission_restrictive();
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(PermCheckingTool {
        permission: perm.clone(),
    })];
    let wrapped = wrap_all(tools, dispatcher, perm);

    let result = wrapped[0].call("ls -la".to_string()).await;
    assert_eq!(result.unwrap(), "ls -la");
}

#[test]
fn wrap_all_returns_original_tools_when_dispatcher_is_empty() {
    let dispatcher = Arc::new(HookDispatcher::from_config(&HashMap::new()).unwrap());
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission());
    assert_eq!(wrapped.len(), 1);
    assert_eq!(wrapped[0].name(), "echo_tool");
}
