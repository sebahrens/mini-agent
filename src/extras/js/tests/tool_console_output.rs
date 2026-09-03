//! JsTool-level contained-worker coverage for model-visible console output and
//! failure diagnostics. The worker collects bounded `console.*` records and a
//! stable `Diagnostic` (stage / script role) on every failure; both must reach
//! the caller instead of being dropped at the tool boundary.

use rig::tool::Tool;

use super::make_test_tool;
use crate::extras::js::tool::JsArgs;

async fn run(code: &str) -> String {
    make_test_tool()
        .call(JsArgs {
            code: code.to_string(),
        })
        .await
        .expect("contained worker call")
}

#[tokio::test]
async fn js_tool_returns_console_records_in_order_with_levels() {
    let result =
        run("console.log('first', 1); console.warn('second'); console.error('third'); 'done'")
            .await;
    assert_eq!(
        result,
        "[console.log] first 1\n[console.warn] second\n[console.error] third\ndone"
    );
}

#[tokio::test]
async fn js_tool_returns_console_output_for_void_results() {
    let result = run("console.log('only')").await;
    assert_eq!(result, "[console.log] only");
}

#[tokio::test]
async fn js_tool_marks_truncated_console_records() {
    let result = run("console.log('x'.repeat(64 * 1024)); 'ok'").await;
    assert!(
        result.contains("[truncated]"),
        "truncated console record must be flagged: {}",
        &result[result.len().saturating_sub(120)..]
    );
    assert!(
        result.ends_with("\nok"),
        "return value must follow console output"
    );
}

#[tokio::test]
async fn js_tool_error_carries_stage_and_script_role_diagnostic_and_console() {
    let result = run("console.log('before failure'); throw new Error('boom')").await;
    assert!(
        result.starts_with("JS error: exception (stage: evaluation; script: model"),
        "unexpected: {result}"
    );
    assert!(
        result.contains("\n[console.log] before failure"),
        "console output emitted before the failure must reach the caller: {result}"
    );
}

#[tokio::test]
async fn js_tool_malformed_source_reports_stage_and_script_role() {
    let result = run("let = ;").await;
    assert!(result.starts_with("JS error: "), "unexpected: {result}");
    assert!(
        result.contains("(stage: evaluation; script: model"),
        "diagnostic stage and script role must be rendered: {result}"
    );
}

#[tokio::test]
async fn js_tool_result_without_console_is_unchanged() {
    assert_eq!(run("21 * 2").await, "42");
    assert_eq!(run("undefined").await, "");
}
