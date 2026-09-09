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
fn exit_zero_without_json_is_no_objection() {
    for stdout in [b"not json at all".as_slice(), b""] {
        let out = output(Some(0), stdout, b"", HookStatus::Completed);
        assert!(matches!(
            interpret_hook_output(&out),
            ChannelResult::NoObjection { json: None }
        ));
    }
}

#[test]
fn block_and_policy_denial_reasons_are_sanitized_and_bounded() {
    let mut input = b"blocked\x1b[31m\x07".to_vec();
    input.extend(std::iter::repeat_n(b'x', 16 * 1024));
    for policy_denied in [false, true] {
        let status = if policy_denied {
            HookStatus::PolicyDenied
        } else {
            HookStatus::Completed
        };
        let out = output(Some(2), b"", &input, status);
        let reason = match (policy_denied, interpret_hook_output(&out)) {
            (false, ChannelResult::Block { stderr }) => stderr,
            (true, ChannelResult::PolicyDenied { reason }) => reason,
            (_, other) => panic!("unexpected channel result: {other:?}"),
        };
        assert!(reason.starts_with("blocked"));
        assert!(!reason.contains('\x1b'));
        assert!(!reason.contains('\x07'));
        assert!(reason.ends_with("[hook reason truncated]"));
        assert!(reason.len() < 8 * 1024 + 64);
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
fn error_exit_codes_are_preserved_for_event_policy() {
    for expected in [Some(1), Some(7), None] {
        let out = output(
            expected,
            b"",
            b"untrusted error detail",
            HookStatus::Completed,
        );
        match interpret_hook_output(&out) {
            ChannelResult::Error { exit_code } => assert_eq!(exit_code, expected),
            other => panic!("expected Error, got {other:?}"),
        }
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
