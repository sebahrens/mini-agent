//! JsTool-level contained-worker coverage for model-visible console output and
//! failure diagnostics. The worker collects bounded `console.*` records and a
//! stable `Diagnostic` (closed class, model location, stage, and script role) on every failure; both must reach
//! the caller instead of being dropped at the tool boundary.

use rig::tool::Tool;

use super::make_test_tool;
use crate::extras::js::tool::JsArgs;

async fn run(code: &str) -> String {
    let audit_dirs = super::TestTempDir::new("js-test-audits");
    let tool = make_test_tool(&audit_dirs);
    tool.call(JsArgs {
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
async fn js_tool_console_renders_objects_as_bounded_json() {
    let result = run("JSON.stringify = () => '\"poison\"'; \
         console.log({answer: 42, nested: [true, null]}, ['a', {b: 2}]); 'done'")
    .await;

    assert_eq!(
        result,
        "[console.log] {\"answer\":42,\"nested\":[true,null]} [\"a\",{\"b\":2}]\ndone"
    );
}

#[tokio::test]
async fn js_tool_console_falls_back_safely_for_circular_and_non_json_objects() {
    let result = run("const circular = {}; circular.self = circular; \
         console.log(circular, new Map([['key', 1]])); 'done'")
    .await;

    assert_eq!(result, "[console.log] [object Object] [object Map]\ndone");
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
        result.starts_with("JS exception at 1:"),
        "unexpected: {result}"
    );
    assert!(
        result.contains("(stage: evaluation; script: model)"),
        "unexpected diagnostic: {result}"
    );
    assert!(
        result.contains("\n[console.log] before failure"),
        "console output emitted before the failure must reach the caller: {result}"
    );
}

#[tokio::test]
async fn js_tool_malformed_source_reports_stage_and_script_role() {
    let result = run("let = ;").await;
    assert!(
        result.starts_with("JS SyntaxError at 1:"),
        "unexpected: {result}"
    );
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

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

#[tokio::test]
async fn js_tool_uncaught_effect_error_names_effect_and_code() {
    let result =
        run("const SECRET_LOCAL = 'S3CR3T_SOURCE_TOKEN'; read_file('nope/missing.txt')").await;
    assert!(result.starts_with("JS exception at 1:"), "{result}");
    assert!(
        result.contains(": read_file: not_found (stage: evaluation; script: model)"),
        "{result}"
    );
    assert!(
        !result.contains("S3CR3T_SOURCE_TOKEN"),
        "source leaked: {result}"
    );
}

#[tokio::test]
async fn js_tool_type_and_reference_errors_carry_the_engine_message() {
    let type_error = run("const r = undefined;\nr.paths").await;
    assert!(type_error.starts_with("JS TypeError at 2:"), "{type_error}");
    assert!(
        type_error.contains("'paths'") && type_error.contains("undefined"),
        "{type_error}"
    );

    let reference = run("missing_identifier_xyz + 1").await;
    assert!(
        reference.starts_with("JS ReferenceError at 1:"),
        "{reference}"
    );
    assert!(
        reference.contains("missing_identifier_xyz is not defined"),
        "{reference}"
    );
}

#[tokio::test]
async fn js_tool_syntax_error_carries_the_engine_message() {
    let result = run("let x = 1;\nlet y = (;").await;
    assert!(result.starts_with("JS SyntaxError at 2:"), "{result}");
    assert!(result.contains("unexpected token"), "{result}");
}

#[tokio::test]
async fn js_tool_error_message_is_sanitized_and_bounded() {
    let result =
        run("throw new Error('first\\nsecond\\u0007\\u001b[31m ' + 'x'.repeat(5000))").await;
    let line = first_line(&result);
    assert!(line.starts_with("JS exception at 1:"), "{line}");
    assert!(
        line.chars().count() <= 300,
        "unbounded: {} chars",
        line.chars().count()
    );
    assert!(result.lines().count() == 1, "newline survived: {result:?}");
    assert!(
        !result.chars().any(char::is_control),
        "control char survived: {result:?}"
    );
    assert!(line.contains("first second"), "{line}");
}

#[tokio::test]
async fn js_tool_limit_errors_state_the_limit_and_remedy() {
    let big = run("'x'.repeat(65537)").await;
    assert!(big.contains("at most 64 KiB"), "{big}");

    let stack = run("function f(){return f()} f()").await;
    assert!(
        stack.contains("stack limit exceeded (512 KiB stack)"),
        "{stack}"
    );

    let effects =
        run("for (let i = 0; i < 300; i++) { try { read_file('nope') } catch (e) {} } 'done'")
            .await;
    assert!(
        effects.contains(
            "effect limit exceeded (256 per call); batch with read_files(paths) or glob()"
        ),
        "{effects}"
    );

    let many = run("read_files(new Array(300).fill('a'))").await;
    assert!(
        many.contains("limit 256 paths per call; split into batches of 256"),
        "{many}"
    );

    let grep_missing = run("grep('x', {path: 'no-such-file.txt'})").await;
    assert!(
        grep_missing.contains("path must be an existing directory or regular file"),
        "{grep_missing}"
    );
    let list_file = run("list_dir('Cargo.toml')").await;
    assert!(
        list_file.contains("path must be a directory; use include for files, or read_file"),
        "{list_file}"
    );
}
