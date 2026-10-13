use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

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
/// shared `PermCheck`, so per-invocation approval routing can be
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
        command: Some("sh".to_string()),
        args: Some(vec!["-c".to_string(), command.to_string()]),
        timeout: Some(5),
        is_async: false,
        condition: None,
        once: false,
        trust: crate::extras::hooks::settings::HookTrust::Trusted,
        env: Default::default(),
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

fn permission_workspace() -> std::path::PathBuf {
    static WORKSPACE: OnceLock<std::path::PathBuf> = OnceLock::new();
    WORKSPACE
        .get_or_init(|| {
            let path = std::env::temp_dir().join(format!(
                "mini-agent-hooks-decorator-workspace-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create permission test workspace");
            path.canonicalize()
                .expect("canonicalize permission test workspace")
        })
        .clone()
}

fn permission() -> Option<crate::permission::checker::PermCheck> {
    Some(Arc::new(std::sync::Mutex::new(
        PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(permission_workspace()),
            None,
        )
        .expect("valid permission test configuration"),
    )))
}

/// Restrictive mode makes an unused or missing one-shot approval observable.
fn permission_restrictive() -> Option<crate::permission::checker::PermCheck> {
    Some(Arc::new(std::sync::Mutex::new(
        PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Restrictive,
            Some(permission_workspace()),
            None,
        )
        .expect("valid permission test configuration"),
    )))
}

#[tokio::test]
async fn deny_blocks_the_call_with_guard_rail_message() {
    let dispatcher = dispatcher_with("PreToolUse", vec![handler("exit 2")]);
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission(), None);

    let result = wrapped[0].call("{}".to_string()).await;
    let err = result.expect_err("expected the call to be blocked");
    assert!(
        err.to_string().contains("Blocked by guard rail"),
        "unexpected error message: {err}"
    );
}

#[tokio::test]
async fn broken_pre_tool_hook_fails_closed() {
    let dispatcher = dispatcher_with(
        "PreToolUse",
        vec![handler("printf 'fixture-secret\\033[31m' >&2; exit 7")],
    );
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission(), None);

    let error = wrapped[0]
        .call("{}".to_string())
        .await
        .expect_err("a failed guard must deny the tool call");
    assert!(error.to_string().contains("PreToolUse hook failed"));
    assert!(!error.to_string().contains("fixture-secret"));
    assert!(!error.to_string().contains('\x1b'));
}

#[tokio::test]
async fn no_matching_hook_passes_through_to_inner_tool() {
    let dispatcher = dispatcher_with("PreToolUse", vec![]);
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission(), None);

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
    let wrapped = wrap_all(tools, dispatcher, permission(), None);

    let result = wrapped[0].call("{}".to_string()).await;
    let err = result.expect_err("inner tool always fails");
    assert!(err.to_string().contains("inner tool blew up"));

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !marker.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("PostToolUseFailure decorator hook did not publish its marker");
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
    let wrapped = wrap_all(tools, dispatcher, permission(), None);

    let result = wrapped[0]
        .call(r#"{"command":"original"}"#.to_string())
        .await
        .unwrap();
    assert_eq!(result, r#"{"command":"rewritten"}"#);
}

#[tokio::test]
async fn hook_approval_describes_the_rewritten_call_and_runs_only_when_approved() {
    use crate::permission::ask::UserDecision;

    for mode in [SecurityMode::Yolo, SecurityMode::Restrictive] {
        for approve in [true, false] {
            let dispatcher = dispatcher_with(
                "PreToolUse",
                vec![handler(
                    r#"echo '{"permissionDecision":"ask","updatedInput":{"command":"echo rewritten"}}'"#,
                )],
            );
            let perm = permission().unwrap();
            perm.lock().unwrap().set_mode(mode);
            let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
            let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(JsonCommandPermCheckingTool {
                permission: Some(perm.clone()),
            })];
            let wrapped = wrap_all(tools, dispatcher, Some(perm), Some(ask_tx));
            let mut call = wrapped[0].call(r#"{"command":"echo original"}"#.into());
            let request = tokio::select! {
                request = ask_rx.recv() => request.expect("approval channel closed"),
                result = &mut call => panic!("call completed before approval: {result:?}"),
            };
            assert_eq!(request.tool, "bash");
            assert_eq!(request.input, r#"{"command":"echo rewritten"}"#);
            request
                .reply
                .send(if approve {
                    UserDecision::AllowOnce
                } else {
                    UserDecision::Deny
                })
                .unwrap();
            let result = call.await;
            if approve {
                assert_eq!(result.unwrap(), r#"{"command":"echo rewritten"}"#);
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("Permission denied by user")
                );
            }
            assert!(
                ask_rx.try_recv().is_err(),
                "one invocation must prompt only once"
            );
        }
    }
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
    let perm = Some(Arc::new(std::sync::Mutex::new(
        PermissionChecker::new(
            &config.into(),
            SecurityMode::Standard,
            Some(permission_workspace()),
            None,
        )
        .expect("valid permission test configuration"),
    )));
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(JsonCommandPermCheckingTool {
        permission: perm.clone(),
    })];
    let wrapped = wrap_all(tools, dispatcher, perm, None);

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
async fn post_tool_use_result_rewrite_is_ignored() {
    let dispatcher = dispatcher_with(
        "PostToolUse",
        vec![handler(r#"echo '{"result":"injected content"}'"#)],
    );
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission(), None);

    let result = wrapped[0]
        .call(r#"{"secret":"abc"}"#.to_string())
        .await
        .unwrap();
    assert_eq!(result, r#"{"secret":"abc"}"#);
}

#[tokio::test]
async fn post_tool_use_can_redact_exact_literals_without_injecting_content() {
    let dispatcher = dispatcher_with(
        "PostToolUse",
        vec![handler(r#"echo '{"redactions":["abc"]}'"#)],
    );
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission(), None);

    let result = wrapped[0]
        .call(r#"{"secret":"abc","public":"ok"}"#.to_string())
        .await
        .unwrap();
    assert_eq!(result, r#"{"secret":"[REDACTED]","public":"ok"}"#);
}

#[tokio::test]
async fn post_tool_use_no_decision_leaves_result_unchanged() {
    let dispatcher = dispatcher_with("PostToolUse", vec![handler("true")]);
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission(), None);

    let result = wrapped[0].call(r#"{"a":1}"#.to_string()).await.unwrap();
    assert_eq!(result, r#"{"a":1}"#);
}

#[tokio::test]
async fn ask_verdict_escalates_to_deny_when_no_ask_tx_is_available() {
    // A hook ask must prompt even when Yolo would otherwise allow the call.
    let dispatcher = dispatcher_with(
        "PreToolUse",
        vec![handler(r#"echo '{"permissionDecision":"ask"}'"#)],
    );
    let perm = permission();
    perm.as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .set_mode(SecurityMode::Yolo);
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(PermCheckingTool {
        permission: perm.clone(),
    })];
    let wrapped = wrap_all(tools, dispatcher, perm, None);

    let result = wrapped[0].call("ls -la".to_string()).await;
    let err = result.expect_err("ask with no ask_tx must escalate to deny");
    assert!(
        err.to_string().contains("non-interactive"),
        "unexpected error message: {err}"
    );
}

#[tokio::test]
async fn allow_verdict_suppresses_prompts_but_preserves_mode_denials() {
    for mode in [
        SecurityMode::Restrictive,
        SecurityMode::ReadOnly,
        SecurityMode::PlanWrite,
    ] {
        let dispatcher = dispatcher_with(
            "PreToolUse",
            vec![handler(r#"echo '{"permissionDecision":"allow"}'"#)],
        );
        let perm = permission();
        perm.as_ref().unwrap().lock().unwrap().set_mode(mode);
        let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(JsonCommandPermCheckingTool {
            permission: perm.clone(),
        })];
        let wrapped = wrap_all(tools, dispatcher, perm, None);
        let result = wrapped[0]
            .call(r#"{"command":"echo harmless"}"#.into())
            .await;
        if mode == SecurityMode::Restrictive {
            assert_eq!(result.unwrap(), r#"{"command":"echo harmless"}"#);
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Permission denied"),
                "{mode:?}"
            );
        }
    }
}

#[tokio::test]
async fn unused_hook_allow_is_cleared_on_completion_failure_and_cancellation() {
    struct WaitingTool {
        entered: tokio::sync::mpsc::Sender<()>,
        release: Arc<tokio::sync::Notify>,
    }
    impl ToolDyn for WaitingTool {
        fn name(&self) -> String {
            "waiting_tool".into()
        }
        fn description(&self) -> String {
            String::new()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
            Box::pin(async move {
                self.entered.send(()).await.unwrap();
                self.release.notified().await;
                if args == "failure" {
                    return Err(ToolError::ToolCallError(Box::new(
                        crate::agent::tools::ToolError::Msg("inner failure".into()),
                    )));
                }
                Ok(args)
            })
        }
    }

    for outcome in ["success", "failure", "cancel"] {
        let dispatcher = dispatcher_with(
            "PreToolUse",
            vec![handler(r#"echo '{"permissionDecision":"allow"}'"#)],
        );
        let perm = permission_restrictive().unwrap();
        // Token zero is reserved by this fixture; production tokens start at one.
        perm.lock().unwrap().allow_once_scoped("other".into(), 0);
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(1);
        let release = Arc::new(tokio::sync::Notify::new());
        let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(WaitingTool {
            entered: entered_tx,
            release: release.clone(),
        })];
        let wrapped = wrap_all(tools, dispatcher, Some(perm.clone()), None);
        let mut call = wrapped[0].call(outcome.into());
        tokio::select! {
            entered = entered_rx.recv() => assert_eq!(entered, Some(())),
            result = &mut call => panic!("tool did not wait: {result:?}"),
        }
        assert_eq!(perm.lock().unwrap().hook_decision_count(), 2);
        if outcome == "cancel" {
            drop(call);
        } else {
            release.notify_one();
            let result = call.await;
            if outcome == "success" {
                assert_eq!(result.unwrap(), "success");
            } else {
                assert!(result.unwrap_err().to_string().contains("inner failure"));
            }
        }
        let checker = perm.lock().unwrap();
        assert_eq!(
            checker.hook_decision_count(),
            1,
            "{outcome} leaked its grant"
        );
        assert!(
            checker.hook_decision_is_pending(0),
            "another invocation lost its grant"
        );
    }
}

#[test]
fn wrap_all_returns_original_tools_when_dispatcher_is_empty() {
    let dispatcher = Arc::new(HookDispatcher::from_config(&HashMap::new()).unwrap());
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission(), None);
    assert_eq!(wrapped.len(), 1);
    assert_eq!(wrapped[0].name(), "echo_tool");
}

// ── Hook decisions are per invocation and always enforced ──────────────

/// A wrapped tool with no permission check at all, like the skills search and
/// advisor tools. Records whether the inner call ran.
struct SideEffectTool {
    ran: Arc<std::sync::atomic::AtomicBool>,
}

impl ToolDyn for SideEffectTool {
    fn name(&self) -> String {
        "side_effect_tool".to_string()
    }

    fn description(&self) -> String {
        String::new()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        let ran = self.ran.clone();
        Box::pin(async move {
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(args)
        })
    }
}

/// Mirrors the Git tool: the public name is `git` while the inner permission
/// check uses an operation-specific key.
struct OperationKeyTool {
    permission: Option<crate::permission::checker::PermCheck>,
    ran: Arc<std::sync::atomic::AtomicBool>,
}

impl ToolDyn for OperationKeyTool {
    fn name(&self) -> String {
        "git".to_string()
    }

    fn description(&self) -> String {
        String::new()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        let ran = self.ran.clone();
        Box::pin(async move {
            crate::agent::tools::check_perm(&self.permission, &None, "git/status", &args)
                .await
                .map_err(|e| ToolError::ToolCallError(Box::new(e)))?;
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(args)
        })
    }
}

fn ask_dispatcher() -> Arc<HookDispatcher> {
    dispatcher_with(
        "PreToolUse",
        vec![handler(r#"echo '{"permissionDecision":"ask"}'"#)],
    )
}

#[tokio::test]
async fn an_ask_verdict_denies_a_tool_that_has_no_inner_permission_check() {
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(SideEffectTool { ran: ran.clone() })];
    let wrapped = wrap_all(tools, ask_dispatcher(), permission(), None);

    let error = wrapped[0]
        .call("{}".to_string())
        .await
        .expect_err("an ask verdict must never fall through");
    assert!(error.to_string().contains("non-interactive"), "{error}");
    assert!(
        !ran.load(std::sync::atomic::Ordering::SeqCst),
        "a denied ask must execute no side effects"
    );
}

#[tokio::test]
async fn an_ask_verdict_denies_a_tool_that_checks_a_different_permission_key() {
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let perm = permission();
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(OperationKeyTool {
        permission: perm.clone(),
        ran: ran.clone(),
    })];
    let wrapped = wrap_all(tools, ask_dispatcher(), perm, None);

    let error = wrapped[0]
        .call("{}".to_string())
        .await
        .expect_err("git/status must not escape the ask verdict for git");
    assert!(error.to_string().contains("non-interactive"), "{error}");
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn an_allow_verdict_is_honored_for_an_operation_specific_key() {
    let dispatcher = dispatcher_with(
        "PreToolUse",
        vec![handler(r#"echo '{"permissionDecision":"allow"}'"#)],
    );
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let perm = permission_restrictive();
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(OperationKeyTool {
        permission: perm.clone(),
        ran: ran.clone(),
    })];
    let wrapped = wrap_all(tools, dispatcher, perm, None);

    wrapped[0]
        .call("{}".to_string())
        .await
        .expect("allow must reach the operation-specific check");
    assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn an_ask_verdict_denies_todo_write_before_it_short_circuits() {
    struct TodoTool {
        permission: Option<crate::permission::checker::PermCheck>,
        ran: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ToolDyn for TodoTool {
        fn name(&self) -> String {
            "todo_write".to_string()
        }
        fn description(&self) -> String {
            String::new()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
            let ran = self.ran.clone();
            Box::pin(async move {
                crate::agent::tools::check_perm(&self.permission, &None, "todo_write", &args)
                    .await
                    .map_err(|e| ToolError::ToolCallError(Box::new(e)))?;
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(args)
            })
        }
    }

    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let perm = permission();
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(TodoTool {
        permission: perm.clone(),
        ran: ran.clone(),
    })];
    let wrapped = wrap_all(tools, ask_dispatcher(), perm, None);

    let error = wrapped[0]
        .call("{}".to_string())
        .await
        .expect_err("todo_write must not short-circuit a hook ask");
    assert!(error.to_string().contains("non-interactive"), "{error}");
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn concurrent_ask_verdicts_deny_every_call() {
    let perm = permission();
    let dispatcher = ask_dispatcher();
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tools: Vec<Box<dyn ToolDyn>> = vec![
        Box::new(SideEffectTool { ran: ran.clone() }),
        Box::new(SideEffectTool { ran: ran.clone() }),
    ];
    let wrapped = wrap_all(tools, dispatcher, perm, None);

    let (first, second) = tokio::join!(
        wrapped[0].call("first".to_string()),
        wrapped[1].call("second".to_string())
    );

    for result in [first, second] {
        let error = result.expect_err("both concurrent asks must deny without an ask channel");
        assert!(error.to_string().contains("non-interactive"), "{error}");
    }
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn a_post_hook_observes_the_arguments_that_actually_ran() {
    let marker = std::env::temp_dir().join(format!(
        "mini-agent-hooks-post-input-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let _ = std::fs::remove_file(&marker);
    let mut config: HashMap<String, Vec<HookGroup>> = HashMap::new();
    config.insert(
        "PreToolUse".to_string(),
        vec![HookGroup {
            matcher: None,
            hooks: vec![handler(
                r#"echo '{"updatedInput":{"path":"actual-target"}}'"#,
            )],
        }],
    );
    config.insert(
        "PostToolUse".to_string(),
        vec![HookGroup {
            matcher: None,
            hooks: vec![handler(&format!("cat > {}", marker.display()))],
        }],
    );
    let dispatcher = Arc::new(HookDispatcher::from_config(&config).unwrap());
    let tools: Vec<Box<dyn ToolDyn>> = vec![Box::new(EchoTool)];
    let wrapped = wrap_all(tools, dispatcher, permission(), None);

    let result = wrapped[0]
        .call(r#"{"path":"original-target"}"#.to_string())
        .await
        .unwrap();
    assert_eq!(result, r#"{"path":"actual-target"}"#);

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("PostToolUse hook did not run");
    let envelope = std::fs::read_to_string(&marker).unwrap();
    let _ = std::fs::remove_file(&marker);
    assert!(
        envelope.contains("actual-target"),
        "post hook must see the executed arguments: {envelope}"
    );
    assert!(
        !envelope.contains("original-target"),
        "post hook must not see the superseded arguments: {envelope}"
    );
}

#[cfg(unix)]
struct McpLikeTool {
    alias: String,
    identity: crate::extras::hooks::normalize::McpToolIdentity,
    permission: Option<crate::permission::checker::PermCheck>,
    effects: Arc<std::sync::atomic::AtomicUsize>,
    waiting: Option<(tokio::sync::mpsc::Sender<()>, Arc<tokio::sync::Notify>)>,
}

#[cfg(unix)]
impl ToolDyn for McpLikeTool {
    fn name(&self) -> String {
        self.alias.clone()
    }
    fn description(&self) -> String {
        "MCP-like singleton dispatch fixture".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            if let Some((entered, release)) = &self.waiting {
                entered.send(()).await.unwrap();
                release.notified().await;
            }
            crate::agent::tools::check_perm(
                &self.permission,
                &None,
                "mcp_tool",
                &self.identity.permission_input(),
            )
            .await
            .map_err(|error| ToolError::ToolCallError(Box::new(error)))?;
            self.effects
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let input: serde_json::Value = serde_json::from_str(&args).unwrap();
            if input["fail"] == true {
                return Err(ToolError::ToolCallError(Box::new(
                    crate::agent::tools::ToolError::Msg(format!(
                        "fixture MCP failure: {}",
                        input["query"]
                    )),
                )));
            }
            Ok(format!("secret-canary: {}", input["query"]))
        })
    }
}

#[cfg(unix)]
fn mcp_wrap(
    tools: Vec<McpLikeTool>,
    dispatcher: Arc<HookDispatcher>,
    permission: Option<crate::permission::checker::PermCheck>,
    ask_tx: Option<crate::permission::ask::AskSender>,
) -> Vec<Box<dyn ToolDyn>> {
    let identities = tools
        .iter()
        .map(|tool| (tool.alias.clone(), tool.identity.clone()))
        .collect();
    crate::extras::hooks::decorator::wrap_all_with_mcp(
        tools
            .into_iter()
            .map(|tool| Box::new(tool) as Box<dyn ToolDyn>)
            .collect(),
        dispatcher,
        permission,
        ask_tx,
        identities,
    )
}

#[cfg(unix)]
fn quote_hook_path(path: &std::path::Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(unix)]
fn mcp_dispatcher(matcher: &str, command: &str) -> Arc<HookDispatcher> {
    Arc::new(
        HookDispatcher::from_config(&HashMap::from([(
            "PreToolUse".into(),
            vec![HookGroup {
                matcher: Some(matcher.into()),
                hooks: vec![handler(command)],
            }],
        )]))
        .unwrap(),
    )
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_mcp_provenance_matches_all_events_and_preserves_model_alias_and_rewrites() {
    use crate::extras::hooks::normalize::McpToolIdentity;
    for (alias, server, tool, matcher) in [
        ("search", "alpha", "search", "mcp__alpha__search"),
        (
            "Alpha_Space__SearchValue__2",
            "Alpha.Space",
            "SearchValue",
            "^mcp__Alpha[.]Space__SearchValue$",
        ),
    ] {
        for fail in [false, true] {
            let root = crate::tests::OwnedTestDirectory::new("mcp-hook-envelopes");
            let pre = root.path().join("pre.json");
            let post = root.path().join("post.json");
            let failure = root.path().join("failure.json");
            let mut config = HashMap::new();
            for (event, path, output) in [
                (
                    "PreToolUse",
                    &pre,
                    serde_json::json!({"permissionDecision":"allow", "updatedInput":{"query":"rewritten", "fail":fail}}),
                ),
                (
                    "PostToolUse",
                    &post,
                    serde_json::json!({"redactions":["secret-canary"]}),
                ),
                (
                    "PostToolUseFailure",
                    &failure,
                    serde_json::json!({"permissionDecision":"deny"}),
                ),
            ] {
                config.insert(
                    event.into(),
                    vec![HookGroup {
                        matcher: Some(matcher.into()),
                        hooks: vec![handler(&format!(
                            "cat > {}; printf '%s\\n' '{}'",
                            quote_hook_path(path),
                            output
                        ))],
                    }],
                );
            }
            let permission = permission_restrictive();
            let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let wrapped = mcp_wrap(
                vec![McpLikeTool {
                    alias: alias.into(),
                    identity: McpToolIdentity {
                        server: server.into(),
                        tool: tool.into(),
                    },
                    permission: permission.clone(),
                    effects: effects.clone(),
                    waiting: None,
                }],
                Arc::new(HookDispatcher::from_config(&config).unwrap()),
                permission.clone(),
                None,
            );
            assert_eq!(wrapped[0].name(), alias);
            let result = wrapped[0].call(r#"{"query":"original"}"#.into()).await;
            assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(
                permission
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .hook_decision_count(),
                0
            );
            if fail {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("fixture MCP failure: \"rewritten\"")
                );
                assert!(!post.exists());
            } else {
                assert_eq!(result.unwrap(), "[REDACTED]: \"rewritten\"");
                assert!(!failure.exists());
            }
            for (event, path) in [
                ("PreToolUse", &pre),
                (
                    if fail {
                        "PostToolUseFailure"
                    } else {
                        "PostToolUse"
                    },
                    if fail { &failure } else { &post },
                ),
            ] {
                let envelope: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                assert_eq!(envelope["hook_event_name"], event);
                assert_eq!(envelope["tool_name"], alias);
                assert_eq!(envelope["mcp_server"], server);
                assert_eq!(envelope["mcp_tool"], tool);
                assert_eq!(
                    envelope["tool_input"]["query"],
                    if event == "PreToolUse" {
                        "original"
                    } else {
                        "rewritten"
                    }
                );
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn hook_ask_without_permission_channel_or_live_reply_has_zero_mcp_effects() {
    use crate::extras::hooks::normalize::McpToolIdentity;
    let identity = McpToolIdentity {
        server: "alpha".into(),
        tool: "search".into(),
    };
    let dispatcher = mcp_dispatcher(
        &identity.matcher_name(),
        r#"printf '%s\n' '{"permissionDecision":"ask"}'"#,
    );
    for (with_permission, with_channel) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let permission = with_permission.then(|| permission_restrictive().unwrap());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        if with_permission && with_channel {
            rx.close();
        }
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wrapped = mcp_wrap(
            vec![McpLikeTool {
                alias: "registered_search".into(),
                identity: identity.clone(),
                permission: permission.clone(),
                effects: effects.clone(),
                waiting: None,
            }],
            dispatcher.clone(),
            permission.clone(),
            with_channel.then_some(tx),
        );
        let error = wrapped[0]
            .call(r#"{"query":"effective"}"#.into())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Permission"), "{error}");
        assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(rx.try_recv().is_err());
        if let Some(permission) = permission {
            assert_eq!(permission.lock().unwrap().hook_decision_count(), 0);
        }
    }
    for decision in [
        None,
        Some(crate::permission::ask::UserDecision::Deny),
        Some(crate::permission::ask::UserDecision::AllowOnce),
    ] {
        let permission = permission_restrictive();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wrapped = mcp_wrap(
            vec![McpLikeTool {
                alias: "registered_search".into(),
                identity: identity.clone(),
                permission: permission.clone(),
                effects: effects.clone(),
                waiting: None,
            }],
            dispatcher.clone(),
            permission.clone(),
            Some(tx),
        );
        let mut call = wrapped[0].call(r#"{"query":"effective"}"#.into());
        let request = tokio::select! {
            request = rx.recv() => request.unwrap(),
            result = &mut call => panic!("call bypassed approval: {result:?}"),
        };
        assert_eq!(request.tool, "registered_search");
        assert_eq!(request.input, r#"{"query":"effective"}"#);
        let approved = matches!(
            &decision,
            Some(crate::permission::ask::UserDecision::AllowOnce)
        );
        if let Some(decision) = decision {
            request.reply.send(decision).unwrap();
        } else {
            drop(request.reply);
        }
        let result = call.await;
        assert_eq!(result.is_ok(), approved);
        assert_eq!(
            effects.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(approved)
        );
        assert_eq!(
            permission
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .hook_decision_count(),
            0
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_explicit_deny_precedes_hook_ask_and_allow_without_prompt_or_effect() {
    use crate::extras::hooks::normalize::McpToolIdentity;
    let identity = McpToolIdentity {
        server: "alpha".into(),
        tool: "search".into(),
    };
    for verdict in ["ask", "allow"] {
        let permission = Some(Arc::new(std::sync::Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(crate::permission::PermissionConfig {
                    mcp_tool: Some(crate::permission::ToolPerm::Granular(HashMap::from([(
                        identity.permission_input(),
                        crate::permission::Action::Deny,
                    )]))),
                    ..Default::default()
                }),
                SecurityMode::Restrictive,
                Some(permission_workspace()),
                None,
            )
            .unwrap(),
        )));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wrapped = mcp_wrap(
            vec![McpLikeTool {
                alias: "renamed_search".into(),
                identity: identity.clone(),
                permission: permission.clone(),
                effects: effects.clone(),
                waiting: None,
            }],
            mcp_dispatcher(
                &identity.matcher_name(),
                &format!("printf '%s\\n' '{{\"permissionDecision\":\"{verdict}\"}}'"),
            ),
            permission.clone(),
            Some(tx),
        );
        assert!(
            wrapped[0]
                .call("{}".into())
                .await
                .unwrap_err()
                .to_string()
                .contains("deny rule")
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            permission
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .hook_decision_count(),
            0
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_mcp_calls_cannot_borrow_another_invocations_hook_grant() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-hook-grants");
    let permission = Some(Arc::new(std::sync::Mutex::new(
        PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Restrictive,
            Some(root.path().to_owned()),
            None,
        )
        .unwrap(),
    )));
    permission
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .allow_once_scoped("stale".into(), 0);
    let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let (entered, mut entered_rx) = tokio::sync::mpsc::channel(1);
    let alpha = crate::extras::hooks::normalize::McpToolIdentity {
        server: "alpha".into(),
        tool: "search".into(),
    };
    let beta = crate::extras::hooks::normalize::McpToolIdentity {
        server: "beta".into(),
        tool: "search".into(),
    };
    let wrapped = mcp_wrap(
        vec![
            McpLikeTool {
                alias: "alpha__search".into(),
                identity: alpha.clone(),
                permission: permission.clone(),
                effects: effects.clone(),
                waiting: Some((entered, release.clone())),
            },
            McpLikeTool {
                alias: "beta__search".into(),
                identity: beta,
                permission: permission.clone(),
                effects: effects.clone(),
                waiting: None,
            },
        ],
        mcp_dispatcher(
            &alpha.matcher_name(),
            r#"printf '%s\n' '{"permissionDecision":"allow"}'"#,
        ),
        permission.clone(),
        None,
    );
    for cancel in [false, true] {
        let mut call = wrapped[0].call(r#"{"query":"alpha"}"#.into());
        tokio::select! {
            entered = entered_rx.recv() => assert_eq!(entered, Some(())),
            result = &mut call => panic!("alpha did not wait: {result:?}"),
        }
        assert_eq!(
            permission
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .hook_decision_count(),
            2
        );
        assert!(wrapped[1].call(r#"{"query":"beta"}"#.into()).await.is_err());
        assert_eq!(
            permission
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .hook_decision_count(),
            2
        );
        if cancel {
            drop(call);
        } else {
            release.notify_one();
            call.await.unwrap();
        }
        let checker = permission.as_ref().unwrap().lock().unwrap();
        assert_eq!(checker.hook_decision_count(), 1);
        assert!(checker.hook_decision_is_pending(0));
    }
    assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn actual_sdk_wrapper_stack_preserves_typed_results_outcomes_and_exact_call_ids() {
    use futures::StreamExt;
    use rig::agent::{AgentBuilder, AgentHook, Flow, HookContext, StepEvent};
    use rig::completion::CompletionModel;
    use rig::streaming::StreamingChat;
    use rig::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig::tool::{Tool, ToolCallExtensions, ToolFailure, ToolReturn};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TypedResult(i64);
    #[derive(Clone)]
    struct TypedInput(i64);
    struct NativeProbe(Arc<std::sync::atomic::AtomicUsize>);
    impl Tool for NativeProbe {
        const NAME: &'static str = "typed_probe";
        type Error = std::convert::Infallible;
        type Args = serde_json::Value;
        type Output = String;
        fn description(&self) -> String {
            "Structured tool fixture".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn call(&self, _args: Self::Args) -> Result<String, Self::Error> {
            Ok("legacy string path".into())
        }
        async fn call_structured(
            &self,
            args: Self::Args,
            extensions: &ToolCallExtensions,
        ) -> Result<ToolReturn<String>, Self::Error> {
            match args["kind"].as_str().unwrap() {
                "timeout" => Ok(ToolReturn::failed(
                    "handled timeout".to_string(),
                    ToolFailure::timeout("fixture timeout"),
                )),
                "denied" => Ok(ToolReturn::denied("fixture refusal".to_string())),
                _ => {
                    self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let input = extensions.get::<TypedInput>().map_or(0, |input| input.0);
                    let value = args["value"].as_i64().unwrap();
                    Ok(ToolReturn::success(format!("secret-canary:{value}"))
                        .with_extension(TypedResult(value + input)))
                }
            }
        }
    }
    struct ObservedResult {
        metadata: Option<TypedResult>,
        provider_id: Option<String>,
        internal_id: String,
        output: String,
        outcome: String,
    }
    #[derive(Clone)]
    struct Observe {
        results: Arc<std::sync::Mutex<Vec<ObservedResult>>>,
        calls: Arc<std::sync::Mutex<HashMap<String, String>>>,
    }
    impl<M: CompletionModel> AgentHook<M> for Observe {
        async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
            match event {
                StepEvent::ToolCall {
                    tool_call_id,
                    internal_call_id,
                    ..
                } => {
                    self.calls.lock().unwrap().insert(
                        tool_call_id
                            .expect("mock provider call must retain its ID")
                            .into(),
                        internal_call_id.into(),
                    );
                }
                StepEvent::ToolResult {
                    extensions,
                    tool_call_id,
                    internal_call_id,
                    result,
                    outcome,
                    ..
                } => {
                    self.results.lock().unwrap().push(ObservedResult {
                        metadata: extensions.get::<TypedResult>().cloned(),
                        provider_id: tool_call_id.map(str::to_owned),
                        internal_id: internal_call_id.into(),
                        output: result.into(),
                        outcome: outcome.as_str().into(),
                    });
                }
                _ => {}
            }
            Flow::cont()
        }
    }
    // A lifecycle-only dispatcher still wraps tools but must preserve the
    // structured fast path without dispatching a tool hook.
    {
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = wrap_all(
            vec![Box::new(NativeProbe(effects.clone()))],
            dispatcher_with("SessionStart", vec![handler("true")]),
            None,
            None,
        );
        let mut extensions = ToolCallExtensions::new();
        extensions.insert(TypedInput(7));
        let result = tools[0]
            .call_structured(r#"{"kind":"success","value":1}"#.into(), &extensions)
            .await;
        assert_eq!(
            result.extensions().get::<TypedResult>(),
            Some(&TypedResult(8))
        );
        assert_eq!(result.model_output(), "secret-canary:1");
        assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    for kind in ["success", "timeout", "denied", "hook-deny"] {
        let root = crate::tests::OwnedTestDirectory::new("hooks-structured-sdk");
        let failure = root.path().join("failure.json");
        let verdict = if kind == "hook-deny" { "deny" } else { "allow" };
        let dispatcher = Arc::new(HookDispatcher::from_config(&HashMap::from([
            ("PreToolUse".into(), vec![HookGroup { matcher: None, hooks: vec![handler(&format!(
                "printf '%s\\n' '{{\"permissionDecision\":\"{verdict}\",\"updatedInput\":{{\"kind\":\"{kind}\",\"value\":42}}}}'"))] }]),
            ("PostToolUse".into(), vec![HookGroup { matcher: None, hooks: vec![handler(r#"printf '%s\n' '{"redactions":["secret-canary"]}'"#)] }]),
            ("PostToolUseFailure".into(), vec![HookGroup { matcher: None, hooks: vec![handler(&format!("cat > {}", quote_hook_path(&failure)))] }]),
        ])).unwrap());
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools =
            crate::agent::tools::memoize::definitions(vec![Box::new(NativeProbe(effects.clone()))]);
        let tools = wrap_all(tools, dispatcher, None, None);
        let tools = crate::agent::tools::concurrency::bind(tools);
        if kind == "success" {
            let mut extensions = ToolCallExtensions::new();
            extensions.insert(TypedInput(7));
            let result = tools[0]
                .call_structured(r#"{"kind":"success","value":1}"#.into(), &extensions)
                .await;
            assert_eq!(
                result.extensions().get::<TypedResult>(),
                Some(&TypedResult(49))
            );
            assert_eq!(result.model_output(), "[REDACTED]:42");
            effects.store(0, std::sync::atomic::Ordering::SeqCst);
        }
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "model-visible-id",
                    "typed_probe",
                    serde_json::json!({"kind":kind, "value":1}),
                )
                .with_call_id("provider-exact-id"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let observed = Observe {
            results: Arc::new(std::sync::Mutex::new(Vec::new())),
            calls: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };
        let agent = AgentBuilder::new(model)
            .tools(tools)
            .add_hook(observed.clone())
            .build();
        let mut stream = agent
            .stream_chat("start", Vec::<rig::completion::Message>::new())
            .max_turns(2)
            .await;
        while let Some(item) = stream.next().await {
            item.expect("mock tool turn completes");
        }
        let rows = observed.results.lock().unwrap();
        assert_eq!(rows.len(), 1);
        let ObservedResult {
            metadata,
            provider_id,
            internal_id,
            output,
            outcome,
        } = &rows[0];
        assert_eq!(provider_id.as_deref(), Some("provider-exact-id"));
        assert!(!internal_id.is_empty());
        assert_eq!(
            observed.calls.lock().unwrap().get("provider-exact-id"),
            Some(internal_id)
        );
        match kind {
            "success" => {
                assert_eq!(metadata, &Some(TypedResult(42)));
                assert_eq!(output, "[REDACTED]:42");
                assert_eq!(outcome, "success");
                assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 1);
                assert!(!failure.exists());
            }
            "timeout" | "denied" => {
                assert!(metadata.is_none());
                assert_eq!(outcome, if kind == "timeout" { "error" } else { "denied" });
                assert_eq!(
                    output,
                    if kind == "timeout" {
                        "handled timeout"
                    } else {
                        "fixture refusal"
                    }
                );
                assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 0);
                let envelope: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&failure).unwrap()).unwrap();
                assert_eq!(envelope["tool_input"]["value"], 42);
                assert_eq!(envelope["error"], output.as_str());
            }
            _ => {
                assert!(metadata.is_none());
                assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 0);
                assert_eq!(outcome, "error");
                assert!(output.contains("Blocked by guard rail"));
                assert!(!failure.exists());
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn delimiter_colliding_mcp_labels_keep_distinct_registration_provenance() {
    use crate::extras::hooks::normalize::McpToolIdentity;
    let root = crate::tests::OwnedTestDirectory::new("mcp-hook-component-collision");
    let path = root.path().join("envelope.json");
    let first = McpToolIdentity {
        server: "a__b".into(),
        tool: "c".into(),
    };
    let second = McpToolIdentity {
        server: "a".into(),
        tool: "b__c".into(),
    };
    assert_eq!(first.matcher_name(), second.matcher_name());
    let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let wrapped = mcp_wrap(
        vec![
            McpLikeTool {
                alias: "allocated_one".into(),
                identity: first.clone(),
                permission: None,
                effects: effects.clone(),
                waiting: None,
            },
            McpLikeTool {
                alias: "allocated_two".into(),
                identity: second.clone(),
                permission: None,
                effects: effects.clone(),
                waiting: None,
            },
        ],
        mcp_dispatcher(
            &first.matcher_name(),
            &format!("cat > {}", quote_hook_path(&path)),
        ),
        None,
        None,
    );
    for (index, identity) in [first, second].iter().enumerate() {
        wrapped[index]
            .call(r#"{"query":"same"}"#.into())
            .await
            .unwrap();
        let envelope: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(envelope["tool_name"], wrapped[index].name());
        assert_eq!(envelope["mcp_server"], identity.server);
        assert_eq!(envelope["mcp_tool"], identity.tool);
    }
    assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 2);
}
