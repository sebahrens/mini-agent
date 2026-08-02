#[cfg(feature = "skills")]
mod agent_skill_catalog;
#[cfg(feature = "skills")]
mod auto_admission_end_to_end;
#[cfg(feature = "skills")]
mod capability_manifest_v2;
#[cfg(feature = "skills")]
mod evidence_policy_scheduler;
#[cfg(feature = "skills")]
mod evidence_promotion_policy;
#[cfg(feature = "skills")]
mod phase5_operations_benchmark;
#[cfg(feature = "skills")]
mod propose_skill_host;
#[cfg(feature = "skills")]
mod self_learning_end_to_end;
#[cfg(feature = "skills")]
mod self_learning_failure_matrix;
#[cfg(feature = "skills")]
mod skill_admission_gate;
#[cfg(feature = "skills")]
mod skill_admission_schema;
#[cfg(feature = "skills")]
mod skill_canary_routing;
#[cfg(feature = "skills")]
mod skill_capability_enforcement;
#[cfg(feature = "skills")]
mod skill_embedder;
#[cfg(feature = "skills")]
mod skill_event_attribution;
#[cfg(feature = "skills")]
mod skill_held_out_evaluator;
#[cfg(feature = "skills")]
mod skill_index;
#[cfg(feature = "skills")]
mod skill_lifecycle_schema;
#[cfg(feature = "skills")]
mod skill_quarantine_policy;
#[cfg(feature = "skills")]
mod skill_realm_isolation;
#[cfg(feature = "skills")]
mod skill_repair_and_rollback;
#[cfg(feature = "skills")]
mod skill_repair_records;
#[cfg(feature = "skills")]
mod skill_retrieval_benchmark;
#[cfg(feature = "skills")]
mod skill_runtime_binding;
#[cfg(feature = "skills")]
mod skill_runtime_prompt;
#[cfg(feature = "skills")]
mod skill_store_identity;
#[cfg(feature = "skills")]
mod skill_store_schema;
#[cfg(feature = "skills")]
mod skill_targeted_feedback;
#[cfg(feature = "skills")]
mod skill_telemetry_retention;
#[cfg(feature = "skills")]
mod skill_verification_semantics;
mod worker_broker;
#[cfg(target_os = "macos")]
mod worker_containment;
mod worker_effect_services;
mod worker_protocol;
mod worker_runtime;

use crate::extras::js::host::AllowConfig;
use crate::extras::js::tool::JsTool;
use crate::extras::js::types::{
    JsOutcome, JsRequest, JsResponse, PermCancellation, STEP_TIMEOUT, THREAD_STACK,
};
use crate::permission::ask::AskSender;
use crate::permission::checker::{PermCheck, PermissionChecker};
use crate::permission::{PermissionConfig, PermissionConfigs, SecurityMode};
use crate::sandbox::Sandbox;

fn make_test_tool() -> JsTool {
    make_test_tool_with_sandbox(Sandbox::new(false, "bwrap"))
}

fn make_test_tool_with_sandbox(sandbox: Sandbox) -> JsTool {
    make_test_tool_with_permissions(sandbox, None, None)
}

fn make_test_tool_with_permissions(
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
) -> JsTool {
    JsTool::new(
        sandbox,
        permission,
        ask_tx,
        AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
    )
}

fn restrictive_permission_allowing_js_entrypoint() -> PermCheck {
    let config = PermissionConfig {
        allow_entries: Some(
            [("js".to_string(), vec!["**".to_string()])]
                .into_iter()
                .collect(),
        ),
        ..PermissionConfig::default()
    };
    std::sync::Arc::new(std::sync::Mutex::new(PermissionChecker::new(
        &PermissionConfigs::from(config),
        SecurityMode::Restrictive,
        std::env::current_dir().ok(),
        Some(vec!["restrictive".to_string()]),
    )))
}

fn restrictive_permission_denying_js_entrypoint() -> PermCheck {
    let config = PermissionConfig {
        deny_entries: Some(
            [("js".to_string(), vec!["**".to_string()])]
                .into_iter()
                .collect(),
        ),
        ..PermissionConfig::default()
    };
    std::sync::Arc::new(std::sync::Mutex::new(PermissionChecker::new(
        &PermissionConfigs::from(config),
        SecurityMode::Restrictive,
        std::env::current_dir().ok(),
        Some(vec!["restrictive".to_string()]),
    )))
}

#[tokio::test]
async fn test_return_value() {
    use rig::tool::Tool;
    let tool = make_test_tool();
    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "1 + 1".to_string(),
        })
        .await
        .expect("call failed");
    assert_eq!(result, "2", "expected '2' but got: {result}");
}

#[tokio::test]
async fn test_fetch_global_matches_sandbox_feature() {
    use rig::tool::Tool;
    let tool = make_test_tool();
    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "typeof fetch".to_string(),
        })
        .await
        .expect("call failed");
    assert_eq!(
        result,
        if cfg!(feature = "sandbox") {
            "function"
        } else {
            "undefined"
        }
    );
}

#[cfg(feature = "sandbox")]
#[tokio::test]
async fn test_fetch_options_fail_closed_before_network_io() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    for (code, expected) in [
        (
            "try { fetch('https://example.com', {method: 'DELETE'}); } catch (e) { String(e) }",
            "method must be GET or POST",
        ),
        (
            "try { fetch('https://example.com', {headers: {Host: 'evil'}}); } catch (e) { String(e) }",
            "header 'host' is controlled by the host",
        ),
        (
            "try { fetch('https://example.com', {unknown: true}); } catch (e) { String(e) }",
            "unsupported field 'unknown'",
        ),
        (
            "try { fetch('https://example.com', {method: 'POST', body: 'x'.repeat(262145)}); } catch (e) { String(e) }",
            "request body exceeds the configured limit",
        ),
    ] {
        let result = tool
            .call(crate::extras::js::tool::JsArgs {
                code: code.to_string(),
            })
            .await
            .expect("call failed");
        assert!(
            result.contains(expected),
            "expected {expected:?} in fetch error, got {result:?}"
        );
    }
}

#[tokio::test]
async fn test_read_write_roundtrip() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let path = std::env::temp_dir().join("zs_test_roundtrip.txt");
    let path_str = path.to_string_lossy().to_string();

    let write_code = format!(
        "write_file({path:?}, 'hello from js'); 'ok'",
        path = path_str
    );
    let write_result = tool
        .call(crate::extras::js::tool::JsArgs { code: write_code })
        .await
        .expect("write call failed");
    assert_eq!(write_result, "ok", "write_file returned: {write_result}");

    let read_code = format!("read_file({path:?})", path = path_str);
    let read_result = tool
        .call(crate::extras::js::tool::JsArgs { code: read_code })
        .await
        .expect("read call failed");
    assert_eq!(
        read_result, "hello from js",
        "read_file returned: {read_result}"
    );
}

#[tokio::test]
async fn test_spawn_captures_output_and_exit_code() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: r#"JSON.stringify(spawn("sh", ["-c", "printf out; printf err >&2; exit 7"]))"#
                .to_string(),
        })
        .await
        .expect("spawn call failed");

    assert_eq!(
        result,
        r#"{"stdout":"out","stderr":"err","code":7,"timed_out":false,"stdout_truncated":false,"stderr_truncated":false}"#
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_spawn_uses_configured_sandbox_wrapper() {
    use rig::tool::Tool;
    let sandbox = Sandbox::new(false, "bwrap").with_shell("false");
    let tool = make_test_tool_with_sandbox(sandbox);

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: r#"JSON.stringify(spawn("printf", ["must not run"]))"#.to_string(),
        })
        .await
        .expect("spawn call failed");

    assert_eq!(
        result,
        r#"{"stdout":"","stderr":"","code":1,"timed_out":false,"stdout_truncated":false,"stderr_truncated":false}"#
    );
}

#[tokio::test]
async fn test_host_globals_enforce_restrictive_permissions() {
    use rig::tool::Tool;

    let permission = restrictive_permission_allowing_js_entrypoint();
    let tool =
        make_test_tool_with_permissions(Sandbox::new(false, "bwrap"), Some(permission), None);
    let path = std::env::temp_dir().join(format!(
        "zs_js_permission_{}_{}.txt",
        std::process::id(),
        line!()
    ));
    let path_str = path.to_string_lossy();

    let read_result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "read_file('Cargo.toml')".to_string(),
        })
        .await
        .expect("read call failed");
    assert!(
        read_result.contains("Permission denied"),
        "read_file bypassed permissions: {read_result}"
    );

    let write_result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: format!("write_file({path_str:?}, 'forbidden')"),
        })
        .await
        .expect("write call failed");
    assert!(
        write_result.contains("Permission denied"),
        "write_file bypassed permissions: {write_result}"
    );
    assert!(
        !path.exists(),
        "denied write_file created {}",
        path.display()
    );

    let spawn_result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: format!("spawn('touch', [{path_str:?}])"),
        })
        .await
        .expect("spawn call failed");
    assert!(
        spawn_result.contains("Permission denied"),
        "spawn bypassed permissions: {spawn_result}"
    );
    assert!(!path.exists(), "denied spawn created {}", path.display());
}

#[tokio::test]
async fn test_timeout() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "while (true) {}".to_string(),
        })
        .await
        .expect("timeout call failed");

    assert_eq!(result, "JS error: execution timed out (30s limit exceeded)");
}

#[tokio::test]
async fn test_exception_includes_stack_trace() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "throw new Error('test exception')".to_string(),
        })
        .await
        .expect("exception call failed");

    assert!(
        result.contains("test exception"),
        "exception message missing from output: {result}"
    );
    assert!(
        result
            .lines()
            .any(|line| line.trim_start().starts_with("at ")),
        "exception stack trace missing from output: {result}"
    );
}

#[tokio::test]
async fn test_permission_denied() {
    use rig::tool::Tool;
    let permission = restrictive_permission_denying_js_entrypoint();
    let tool =
        make_test_tool_with_permissions(Sandbox::new(false, "bwrap"), Some(permission), None);

    let error = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "1 + 1".to_string(),
        })
        .await
        .expect_err("denied JavaScript unexpectedly executed");

    assert!(
        error.to_string().contains("Permission denied"),
        "unexpected permission error: {error}"
    );
}

#[tokio::test]
async fn test_oom() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "const chunks = []; while (true) { chunks.push(new ArrayBuffer(1024 * 1024)); }"
                .to_string(),
        })
        .await
        .expect("OOM call failed");

    assert_eq!(
        result, "JS error: out of memory (64 MiB limit exceeded)",
        "memory limit did not produce the classified OOM response"
    );
}

#[tokio::test]
async fn js_outcome_mapping() {
    use std::time::Duration;

    use crate::extras::js::engine::run_step_for_test;
    use crate::extras::js::tool::PermissionBridgeOwner;

    let sandbox = Sandbox::new(false, "bwrap");
    let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
    let bridge = owner.bridge();
    let runtime = tokio::runtime::Handle::current();
    let allow_config = AllowConfig::unrestricted(&std::env::current_dir().unwrap());

    let run = |code: &str, timeout: Duration, max_pending_jobs: usize| {
        run_step_for_test(
            code,
            &sandbox,
            &bridge,
            &PermCancellation::new(),
            &runtime,
            &allow_config,
            timeout,
            max_pending_jobs,
        )
    };
    let normal_timeout = Duration::from_secs(2);
    let assert_recovers = || {
        assert_eq!(
            run("1 + 1", normal_timeout, 10_000),
            JsOutcome::Value("2".to_string())
        );
    };

    assert_eq!(
        run(
            "const chunks = []; while (true) { chunks.push(new ArrayBuffer(1024 * 1024)); }",
            STEP_TIMEOUT,
            10_000,
        ),
        JsOutcome::OomKilled
    );
    assert_recovers();

    assert_eq!(
        run("while (true) {}", Duration::from_millis(50), 10_000),
        JsOutcome::Timeout
    );
    assert_recovers();

    assert_eq!(
        run(
            "function spin() { Promise.resolve().then(spin); } spin();",
            normal_timeout,
            1_000,
        ),
        JsOutcome::Timeout
    );
    assert_recovers();

    for (code, expected) in [("throw 'x'", "x"), ("throw 1", "1"), ("throw null", "null")] {
        assert_eq!(
            run(code, normal_timeout, 10_000),
            JsOutcome::Error(expected.to_string())
        );
        assert_recovers();
    }

    let object_error = match run("throw new Error('object failure')", normal_timeout, 10_000) {
        JsOutcome::Error(error) => error,
        outcome => panic!("object throw did not return JsOutcome::Error: {outcome:?}"),
    };
    assert!(object_error.starts_with("object failure\n"));
    assert!(
        object_error
            .lines()
            .any(|line| line.trim().starts_with("at "))
    );
    assert_recovers();

    match run("throw new Error('out of memory')", normal_timeout, 10_000) {
        JsOutcome::Error(_) => {}
        outcome => {
            panic!("ordinary errors mentioning memory were misclassified as OOM: {outcome:?}")
        }
    }
    assert_recovers();

    let syntax_error = match run("function (", normal_timeout, 10_000) {
        JsOutcome::Error(error) => error,
        outcome => panic!("syntax error did not return JsOutcome::Error: {outcome:?}"),
    };
    assert!(!syntax_error.is_empty());
    assert_recovers();

    assert_eq!(
        run("Promise.reject('rejected')", normal_timeout, 10_000),
        JsOutcome::Error("rejected".to_string())
    );
    assert_recovers();

    let rejected_object = match run(
        "Promise.reject(new Error('rejected object'))",
        normal_timeout,
        10_000,
    ) {
        JsOutcome::Error(error) => error,
        outcome => panic!("object rejection did not return JsOutcome::Error: {outcome:?}"),
    };
    assert!(rejected_object.starts_with("rejected object\n"));
    assert!(
        rejected_object
            .lines()
            .any(|line| line.trim().starts_with("at "))
    );
    assert_recovers();

    assert_eq!(
        run(
            "Promise.resolve().then(() => { throw 7; })",
            normal_timeout,
            10_000,
        ),
        JsOutcome::Error("7".to_string())
    );
    assert_recovers();

    assert_eq!(
        run(
            "globalThis.leaked = 42; throw 'failed'",
            normal_timeout,
            10_000,
        ),
        JsOutcome::Error("failed".to_string())
    );
    assert_eq!(
        run("typeof globalThis.leaked", normal_timeout, 10_000),
        JsOutcome::Value("undefined".to_string())
    );

    owner.shutdown();
}

#[tokio::test]
async fn runtime_has_no_require_or_dynamic_module_loader() {
    use std::time::Duration;

    use crate::extras::js::engine::run_step_for_test;
    use crate::extras::js::tool::PermissionBridgeOwner;

    let sandbox = Sandbox::new(false, "bwrap");
    let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
    let bridge = owner.bridge();
    let runtime = tokio::runtime::Handle::current();
    let allow_config = AllowConfig::unrestricted(&std::env::current_dir().unwrap());
    let run = |code: &str| {
        run_step_for_test(
            code,
            &sandbox,
            &bridge,
            &PermCancellation::new(),
            &runtime,
            &allow_config,
            Duration::from_secs(2),
            10_000,
        )
    };

    assert_eq!(
        run("typeof require"),
        JsOutcome::Value("undefined".to_string())
    );
    for dynamic_import in [
        "import('file:///tmp/mini-agent-no-loader.js')",
        "import('file:///tmp/mini-agent-native-loader.so')",
    ] {
        assert!(
            matches!(run(dynamic_import), JsOutcome::Error(_)),
            "dynamic import unexpectedly resolved without a configured loader"
        );
    }
    assert!(
        matches!(
            run("import value from 'file:///tmp/mini-agent-no-loader.js'; value"),
            JsOutcome::Error(_)
        ),
        "an import declaration entered the script-only evaluation path"
    );

    owner.shutdown();
}

#[tokio::test]
async fn test_js_reply_receiver_drop_is_non_fatal() {
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::extras::js::engine::js_thread_main;
    use crate::extras::js::tool::PermissionBridgeOwner;

    let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
    let bridge = owner.bridge();
    let (request_tx, request_rx) = mpsc::channel();
    let runtime = tokio::runtime::Handle::current();
    let js_thread = std::thread::Builder::new()
        .name("js-reply-drop-test".into())
        .stack_size(THREAD_STACK)
        .spawn(move || {
            js_thread_main(
                request_rx,
                Sandbox::new(false, "bwrap"),
                bridge,
                runtime,
                AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
                crate::extras::js::engine::NormalExecutionHosts::default(),
            );
        })
        .expect("failed to spawn JS reply-drop test thread");

    let (completed_reply, completed_receiver) = tokio::sync::oneshot::channel::<JsResponse>();
    drop(completed_receiver);
    request_tx
        .send(JsRequest {
            code: "1 + 1".to_string(),
            #[cfg(feature = "skills")]
            skill_bundle: std::sync::Arc::new(
                crate::extras::js::skills::turn::TurnSkillBundle::empty("test"),
            ),
            #[cfg(feature = "skills")]
            skill_tool_call_id: "completed-tool".to_string(),
            cancellation: PermCancellation::new(),
            reply: completed_reply,
        })
        .expect("failed to send normal-completion request");

    let cancellation = PermCancellation::new();
    cancellation.cancel();
    let (cancelled_reply, cancelled_receiver) = tokio::sync::oneshot::channel::<JsResponse>();
    drop(cancelled_receiver);
    request_tx
        .send(JsRequest {
            code: "throw new Error('must not execute')".to_string(),
            #[cfg(feature = "skills")]
            skill_bundle: std::sync::Arc::new(
                crate::extras::js::skills::turn::TurnSkillBundle::empty("test"),
            ),
            #[cfg(feature = "skills")]
            skill_tool_call_id: "cancelled-tool".to_string(),
            cancellation,
            reply: cancelled_reply,
        })
        .expect("failed to send early-cancel request");

    let (recovery_reply, recovery_receiver) = tokio::sync::oneshot::channel();
    request_tx
        .send(JsRequest {
            code: "42".to_string(),
            #[cfg(feature = "skills")]
            skill_bundle: std::sync::Arc::new(
                crate::extras::js::skills::turn::TurnSkillBundle::empty("test"),
            ),
            #[cfg(feature = "skills")]
            skill_tool_call_id: "recovery-tool".to_string(),
            cancellation: PermCancellation::new(),
            reply: recovery_reply,
        })
        .expect("failed to send recovery request");

    let recovery = tokio::time::timeout(Duration::from_secs(5), recovery_receiver)
        .await
        .expect("JS thread stopped after a dropped receiver")
        .expect("JS thread closed the recovery reply channel");
    assert_eq!(recovery.outcome, JsOutcome::Value("42".to_string()));

    drop(request_tx);
    js_thread
        .join()
        .expect("JS reply-drop test thread panicked");
    owner.shutdown();
}
