use crate::extras::js::tool::JsTool;
use crate::extras::js::types::{JsOutcome, PermCancellation, STEP_TIMEOUT};
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
    JsTool::new(sandbox, permission, ask_tx)
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

    let run = |code: &str, timeout: Duration, max_pending_jobs: usize| {
        run_step_for_test(
            code,
            &sandbox,
            &bridge,
            &PermCancellation::new(),
            &runtime,
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
async fn test_drop_recovery_after_timeout() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let timeout_result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "while (true) {}".to_string(),
        })
        .await
        .expect("timeout call failed");
    assert_eq!(
        timeout_result, "JS error: execution timed out (30s limit exceeded)",
        "first call should time out: {timeout_result}"
    );

    let recovery_result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "35411".to_string(),
        })
        .await
        .expect("recovery call failed");
    assert_eq!(recovery_result, "35411", "recovery should return 35411");
}

#[tokio::test]
async fn test_js_reply_drop_recovery_699() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let timeout_result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "while (true) {}".to_string(),
        })
        .await
        .expect("timeout call failed");
    assert_eq!(
        timeout_result, "JS error: execution timed out (30s limit exceeded)",
        "first call should time out: {timeout_result}"
    );

    let recovery_result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "699".to_string(),
        })
        .await
        .expect("recovery call failed");
    assert_eq!(recovery_result, "699", "recovery should return 699");
}
