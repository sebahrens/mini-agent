use crate::extras::hooks::channel::{ChannelResult, interpret_hook_output};
use crate::extras::hooks::subprocess::{HookDiagnostics, HookOutput, HookStatus, OutputLimit};

fn output(exit_code: Option<i32>, stdout: &[u8], stderr: &[u8], status: HookStatus) -> HookOutput {
    HookOutput {
        started: true,
        exit_code,
        stdout: stdout.to_vec(),
        stderr: stderr.to_vec(),
        status,
        diagnostics: HookDiagnostics {
            containment: "test",
            environment: "test",
            filesystem: "test",
            network: "test",
        },
    }
}

#[test]
fn exit_zero_with_valid_json_returns_the_json() {
    let out = output(
        Some(0),
        br#"{"permissionDecision":"allow"}"#,
        b"",
        HookStatus::Completed,
    );
    match interpret_hook_output(&out) {
        ChannelResult::NoObjection { json: Some(v) } => {
            assert_eq!(v["permissionDecision"], "allow");
        }
        other => panic!("expected NoObjection with json, got {other:?}"),
    }
}

#[test]
fn exit_zero_with_non_json_stdout_is_ignored() {
    let out = output(Some(0), b"not json at all", b"", HookStatus::Completed);
    assert!(matches!(
        interpret_hook_output(&out),
        ChannelResult::NoObjection { json: None }
    ));
}

#[test]
fn exit_zero_with_empty_stdout_is_no_objection() {
    let out = output(Some(0), b"", b"", HookStatus::Completed);
    assert!(matches!(
        interpret_hook_output(&out),
        ChannelResult::NoObjection { json: None }
    ));
}

#[test]
fn exit_two_blocks_with_stderr_feedback() {
    let out = output(
        Some(2),
        b"",
        b"denied: dangerous command",
        HookStatus::Completed,
    );
    match interpret_hook_output(&out) {
        ChannelResult::Block { stderr } => assert_eq!(stderr, "denied: dangerous command"),
        other => panic!("expected Block, got {other:?}"),
    }
}

#[test]
fn exit_two_with_json_also_present_ignores_the_json() {
    let out = output(
        Some(2),
        br#"{"permissionDecision":"allow"}"#,
        b"denied",
        HookStatus::Completed,
    );
    match interpret_hook_output(&out) {
        ChannelResult::Block { stderr } => assert_eq!(stderr, "denied"),
        other => panic!("expected Block (json ignored), got {other:?}"),
    }
}

#[test]
fn other_exit_code_is_non_blocking_error() {
    let out = output(Some(1), b"", b"boom", HookStatus::Completed);
    match interpret_hook_output(&out) {
        ChannelResult::Error { exit_code, stderr } => {
            assert_eq!(exit_code, Some(1));
            assert_eq!(stderr, "boom");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn timeout_is_reported_distinctly() {
    let out = output(None, b"", b"", HookStatus::TimedOut);
    assert!(matches!(
        interpret_hook_output(&out),
        ChannelResult::TimedOut
    ));
}

#[test]
fn output_limit_is_reported_distinctly() {
    let out = output(
        None,
        b"partial",
        b"",
        HookStatus::OutputLimitExceeded(OutputLimit::Stdout),
    );
    assert!(matches!(
        interpret_hook_output(&out),
        ChannelResult::OutputLimitExceeded
    ));
}
