use std::sync::mpsc;

use crate::extras::js::{
    engine::js_thread_main,
    tool::JsTool,
    types::THREAD_STACK,
};
use crate::sandbox::Sandbox;

fn make_test_tool() -> JsTool {
    make_test_tool_with_sandbox(Sandbox::new(false, "bwrap"))
}

fn make_test_tool_with_sandbox(sandbox: Sandbox) -> JsTool {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("js-engine-test".into())
        .stack_size(THREAD_STACK)
        .spawn(move || js_thread_main(rx, sandbox))
        .expect("failed to spawn JS test thread");
    JsTool::new(tx, None, None)
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
