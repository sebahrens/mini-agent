use std::sync::mpsc;

use crate::extras::js::{engine::js_thread_main, tool::JsTool, types::THREAD_STACK};
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
    let (tx, rx) = mpsc::channel();
    let js_permission = permission.clone();
    let js_ask_tx = ask_tx.clone();
    let runtime = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("js-engine-test".into())
        .stack_size(THREAD_STACK)
        .spawn(move || js_thread_main(rx, sandbox, js_permission, js_ask_tx, runtime))
        .expect("failed to spawn JS test thread");
    JsTool::new(tx, permission, ask_tx)
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

    assert_eq!(result, r#"{"stdout":"out","stderr":"err","code":7}"#);
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

    assert_eq!(result, r#"{"stdout":"","stderr":"","code":1}"#);
}

#[tokio::test]
async fn test_host_globals_enforce_restrictive_permissions() {
    use rig::tool::Tool;

    let permission = restrictive_permission_allowing_js_entrypoint();
    let tool = make_test_tool_with_permissions(
        Sandbox::new(false, "bwrap"),
        Some(permission),
        None,
    );
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
    assert!(!path.exists(), "denied write_file created {}", path.display());

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
